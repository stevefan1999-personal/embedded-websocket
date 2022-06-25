// The MIT License (MIT)
// Copyright (c) 2019 David Haig

// Demo websocket server that listens on localhost port 1337.
// If accessed from a browser it will return a web page that will automatically attempt to
// open a websocket connection to itself. Alternatively, the main example can be used to
// open a websocket connection directly. The server will echo all Text and Ping messages back to
// the client as well as responding to any opening and closing handshakes.
// Note that we are using the standard library in the demo but the websocket library remains no_std

#![feature(core_intrinsics)]

use embedded_io::{adapters::FromTokio, asynch::Read as AsyncRead, asynch::Write as AsyncWrite};
use embedded_tls::{Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext, TlsError};
use embedded_websocket as ws;
use httparse::Request;
use once_cell::sync::Lazy;
use rand_core::OsRng;
use route_recognizer::Router;
use std::io;
use std::str::Utf8Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio_stream::{wrappers::TcpListenerStream, StreamExt};
use ws::{
    framer::{Framer, FramerError, ReadResult},
    WebSocketSendMessageType, WebSocketServer,
};

type Result<T> = std::result::Result<T, WebServerError>;

#[derive(Debug)]
pub enum WebServerError {
    Io(io::Error),
    Framer(FramerError<io::Error>),
    WebSocket(ws::Error),
    HttpError(httparse::Error),
    Utf8Error,
    TlsError(TlsError),
    Custom(String),
}

fn to_io_error<T: core::fmt::Debug>(err: T) -> io::Error {
    let kind = io::ErrorKind::Other;
    io::Error::new(kind, format!("{:?}", err))
}

impl From<io::Error> for WebServerError {
    fn from(err: io::Error) -> WebServerError {
        WebServerError::Io(err)
    }
}

impl From<FramerError<io::Error>> for WebServerError {
    fn from(err: FramerError<io::Error>) -> WebServerError {
        WebServerError::Framer(err)
    }
}

impl From<FramerError<TlsError>> for WebServerError {
    fn from(err: FramerError<TlsError>) -> WebServerError {
        WebServerError::Framer(FramerError::Io(to_io_error(err)))
    }
}

impl From<ws::Error> for WebServerError {
    fn from(err: ws::Error) -> WebServerError {
        WebServerError::WebSocket(err)
    }
}

impl From<Utf8Error> for WebServerError {
    fn from(_: Utf8Error) -> WebServerError {
        WebServerError::Utf8Error
    }
}

impl From<TlsError> for WebServerError {
    fn from(err: TlsError) -> WebServerError {
        WebServerError::TlsError(err)
    }
}

impl From<httparse::Error> for WebServerError {
    fn from(err: httparse::Error) -> WebServerError {
        WebServerError::HttpError(err)
    }
}

// type TlsStream<'a> = TlsConnection<'a, FromTokio<TcpStream>, Aes128GcmSha256>;

type TlsStream = FromTokio<TcpStream>;

#[tokio::main]
async fn main() -> io::Result<()> {
    // SimpleLogger::new().init().unwrap();

    let addr = "127.0.0.1:1337";
    let listener = TcpListener::bind(addr).await?;
    println!("Listening on: {}", addr);

    let mut incoming = {
        cfg_if::cfg_if! {
            if #[cfg(feature = "example-tokio")] {
                 TcpListenerStream::new(listener)
            } else {
                 listener.incoming()
            }
        }
    };

    while let Some(stream) = incoming.next().await {
        match stream {
            Ok(stream) => {
                let fut = || async {
                    println!("Client connected {:?}", stream.peer_addr());
                    let stream = FromTokio::new(stream);

                    // let mut record_buffer = [0; 16384];
                    // let config = TlsConfig::new()
                    //     .with_server_name("localhost")
                    //     .verify_hostname(false)
                    //     .verify_cert(false);
                    // let mut rng = OsRng;
                    // let mut tls = TlsConnection::new(stream, &mut record_buffer);
                    //
                    // let context = TlsContext::new(&config, &mut rng);
                    // tls.open::<OsRng, std::time::SystemTime, 4096>(context)
                    //     .await
                    //     .map_err(to_io_error)?;

                    handle_client(stream).await
                };

                tokio::task::spawn_blocking(move || {
                    let local = tokio::task::LocalSet::new();
                    let mut rt = Runtime::new().unwrap();

                    match local.block_on(&mut rt, fut()) {
                        Ok(()) => println!("Connection closed"),
                        Err(e) => println!("Error: {:?}", e),
                    }
                });
            }
            Err(e) => println!("Failed to establish a connection: {}", e),
        }
    }

    Ok(())
}

static ROUTER: Lazy<Router<&'static str>> = Lazy::new(|| {
    let mut router = Router::new();
    router.add("/chat", "/chat");
    router.add("/", "/");
    router
});

async fn handle_client(mut stream: TlsStream) -> Result<()> {
    let mut read_buf = [0; 4000];
    let mut read_cursor = 0;

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let received_size = stream.read(&mut read_buf[read_cursor..]).await?;
    let request = loop {
        let mut request = Request::new(&mut headers);
        match request.parse(&read_buf[..read_cursor + received_size]) {
            Ok(httparse::Status::Partial) => read_cursor += received_size,
            Ok(httparse::Status::Complete(_)) => break request,
            // Err(httparse::Error::TooManyHeaders) => {
            //     headers = headers.resize(headers.len() * 2, httparse::EMPTY_HEADER)
            // }
            Err(e) => return Err(e.into()),
        }
    };

    match ROUTER.recognize(request.path.unwrap_or("/")) {
        Ok(handler) => match **handler.handler() {
            "/chat" => {
                println!("Received chat request: {:?}", request.path);

                if let Some(websocket_context) =
                    ws::read_http_header(request.headers.iter().map(|f| (f.name, f.value)))?
                {
                    // this is a websocket upgrade HTTP request
                    let mut read_buf = [0; 4000];
                    let mut read_cursor = 0;
                    let mut write_buf = [0; 4000];
                    let mut frame_buf = [0; 4000];
                    let mut websocket = WebSocketServer::new_server();
                    let mut framer = Framer::new(
                        &mut read_buf,
                        &mut read_cursor,
                        &mut write_buf,
                        &mut websocket,
                    );

                    // complete the opening handshake with the client
                    framer.accept_async(&mut stream, &websocket_context).await?;
                    println!("Websocket connection opened");

                    // read websocket frames
                    while let ReadResult::Text(text) =
                        framer.read_async(&mut stream, &mut frame_buf).await?
                    {
                        println!("Received: {}", text);

                        // send the text back to the client
                        framer
                            .write_async(
                                &mut stream,
                                WebSocketSendMessageType::Text,
                                true,
                                format!("hello {}", text).as_bytes(),
                            )
                            .await?
                    }

                    println!("Closing websocket connection");
                }

                Ok(())
            }
            "/" => {
                stream.write_all(&ROOT_HTML.as_bytes()).await?;
                Ok(())
            }
            _ => unreachable!(),
        },
        Err(e) => {
            println!("Unknown path: {:?}", request.path);
            let html = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{msg}",
                len = e.len(),
                msg = e
            );
            stream.write_all(&html.as_bytes()).await?;
            Err(WebServerError::Custom(e))
        }
    }
}

const ROOT_HTML : &str = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: 2590\r\nConnection: close\r\n\r\n<!doctype html>
<html>
<head>
    <meta content='text/html;charset=utf-8' http-equiv='Content-Type' />
    <meta content='utf-8' http-equiv='encoding' />
    <meta name='viewport' content='width=device-width, initial-scale=0.5, maximum-scale=0.5, user-scalable=0' />
    <meta name='apple-mobile-web-app-capable' content='yes' />
    <meta name='apple-mobile-web-app-status-bar-style' content='black' />
    <title>Web Socket Demo</title>
    <style type='text/css'>
        * { margin: 0; padding: 0; box-sizing: border-box; }
        body { font: 13px Helvetica, Arial; }
        form { background: #000; padding: 3px; position: fixed; bottom: 0; width: 100%; }
        form input { border: 0; padding: 10px; width: 90%; margin-right: .5%; }
        form button { width: 9%; background: rgb(130, 200, 255); border: none; padding: 10px; }
        #messages { list-style-type: none; margin: 0; padding: 0; }
        #messages li { padding: 5px 10px; }
        #messages li:nth-child(odd) { background: #eee; }
    </style>
</head>
<body>
    <ul id='messages'></ul>
    <form action=''>
    <input id='txtBox' autocomplete='off' /><button>Send</button>
    </form>
    <script type='text/javascript' src='http://code.jquery.com/jquery-1.11.1.js' ></script>
    <script type='text/javascript'>
        var CONNECTION;
        window.onload = function () {
            // open the connection to the Web Socket server
            CONNECTION = new WebSocket('ws://localhost:1337/chat');
			// CONNECTION = new WebSocket('ws://' + location.host + ':1337/chat');

            // When the connection is open
            CONNECTION.onopen = function () {
                $('#messages').append($('<li>').text('Connection opened'));
            };

            // when the connection is closed by the server
            CONNECTION.onclose = function () {
                $('#messages').append($('<li>').text('Connection closed'));
            };

            // Log errors
            CONNECTION.onerror = function (e) {
                console.log('An error occured');
            };

            // Log messages from the server
            CONNECTION.onmessage = function (e) {
                $('#messages').append($('<li>').text(e.data));
            };
        };

		$(window).on('beforeunload', function(){
			CONNECTION.close();
		});

        // when we press the Send button, send the text to the server
        $('form').submit(function(){
            CONNECTION.send($('#txtBox').val());
            $('#txtBox').val('');
            return false;
        });
    </script>
</body>
</html>";
