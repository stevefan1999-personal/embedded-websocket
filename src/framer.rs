// This module helps you work with the websocket library using a stream of data rather than using it as a raw codec.
// This is the most common use case when working with websockets and is recommended due to the hand shaky nature of
// the protocol as well as the fact that an input buffer can contain multiple websocket frames or maybe only a fragment of one.
// This module allows you to work with discrete websocket frames rather than the multiple fragments you read off a stream.
// NOTE: if you are using the standard library then you can use the built in Read and Write traits from std otherwise
//       you have to implement the Read and Write traits specified below

use crate::{
    WebSocket, WebSocketCloseStatusCode, WebSocketContext, WebSocketOptions,
    WebSocketReceiveMessageType, WebSocketSendMessageType, WebSocketState, WebSocketSubProtocol,
    WebSocketType,
};
use core::{cmp::min, str::Utf8Error};
use rand_core::RngCore;

pub trait Read<E> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, E>;
}

pub trait Write<E> {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), E>;
}

cfg_if::cfg_if! {
    if #[cfg(feature = "std")] {
        impl Read<std::io::Error> for std::net::TcpStream {
            fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
                std::io::Read::read(self, buf)
            }
        }

        impl Write<std::io::Error> for std::net::TcpStream {
            fn write_all(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
                std::io::Write::write_all(self, buf)
            }
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(feature = "async")] {
        #[async_trait::async_trait]
        pub trait AsyncRead<E> {
            async fn read(&mut self, buf: &mut [u8]) -> Result<usize, E>;
        }

        #[async_trait::async_trait]
        pub trait AsyncWrite<E> {
            async fn write_all(&mut self, buf: &[u8]) -> Result<(), E>;
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(feature = "tokio")] {
        #[async_trait::async_trait]
        impl AsyncRead<std::io::Error> for tokio::net::TcpStream {
            async fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
                tokio::io::AsyncReadExt::read(self, buf).await
            }
        }

        #[async_trait::async_trait]
        impl AsyncWrite<std::io::Error> for tokio::net::TcpStream {
            async fn write_all(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
                tokio::io::AsyncWriteExt::write_all(self, buf).await
            }
        }
    } else if #[cfg(feature = "smol")] {
        #[async_trait::async_trait]
        impl AsyncRead<std::io::Error> for smol::net::TcpStream {
            async fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
                smol::io::AsyncReadExt::read(self, buf).await
            }
        }

        #[async_trait::async_trait]
        impl AsyncWrite<std::io::Error> for smol::net::TcpStream {
            async fn write_all(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
                smol::io::AsyncWriteExt::write_all(self, buf).await
            }
        }
    } else if #[cfg(feature = "async-std")] {
        #[async_trait::async_trait]
        impl AsyncRead<std::io::Error> for async_std::net::TcpStream {
            async fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
                async_std::io::ReadExt::read(self, buf).await
            }
        }

        #[async_trait::async_trait]
        impl AsyncWrite<std::io::Error> for async_std::net::TcpStream {
            async fn write_all(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
                async_std::io::WriteExt::write_all(self, buf).await
            }
        }
    }
}

pub enum ReadResult<'a> {
    Binary(&'a [u8]),
    Text(&'a str),
    Pong(&'a [u8]),
    Closed,
}

#[derive(Debug)]
pub enum FramerError<E> {
    Io(E),
    FrameTooLarge(usize),
    Utf8(Utf8Error),
    HttpHeader(httparse::Error),
    WebSocket(crate::Error),
}

pub struct Framer<'a, TRng, TWebSocketType>
where
    TRng: RngCore,
    TWebSocketType: WebSocketType,
{
    read_buf: &'a mut [u8],
    write_buf: &'a mut [u8],
    read_cursor: &'a mut usize,
    frame_cursor: usize,
    read_len: usize,
    websocket: &'a mut WebSocket<TRng, TWebSocketType>,
}

impl<'a, TRng> Framer<'a, TRng, crate::Client>
where
    TRng: RngCore,
{
    pub fn connect<E>(
        &mut self,
        stream: &mut (impl Read<E> + Write<E>),
        websocket_options: &WebSocketOptions,
    ) -> Result<Option<WebSocketSubProtocol>, FramerError<E>> {
        let (len, web_socket_key) = self
            .websocket
            .client_connect(websocket_options, &mut self.write_buf)
            .map_err(FramerError::WebSocket)?;
        stream
            .write_all(&self.write_buf[..len])
            .map_err(FramerError::Io)?;
        *self.read_cursor = 0;

        loop {
            // read the response from the server and check it to complete the opening handshake
            let received_size = stream
                .read(&mut self.read_buf[*self.read_cursor..])
                .map_err(FramerError::Io)?;

            match self.websocket.client_accept(
                &web_socket_key,
                &self.read_buf[..*self.read_cursor + received_size],
            ) {
                Ok((len, sub_protocol)) => {
                    // "consume" the HTTP header that we have read from the stream
                    // read_cursor would be 0 if we exactly read the HTTP header from the stream and nothing else
                    *self.read_cursor += received_size - len;
                    return Ok(sub_protocol);
                }
                Err(crate::Error::HttpHeaderIncomplete) => {
                    *self.read_cursor += received_size;
                    // continue reading HTTP header in loop
                }
                Err(e) => {
                    *self.read_cursor += received_size;
                    return Err(FramerError::WebSocket(e));
                }
            }
        }
    }
}

impl<'a, TRng> Framer<'a, TRng, crate::Server>
where
    TRng: RngCore,
{
    pub fn accept<E>(
        &mut self,
        stream: &mut impl Write<E>,
        websocket_context: &WebSocketContext,
    ) -> Result<(), FramerError<E>> {
        let len = self.accept_len(&websocket_context)?;

        stream
            .write_all(&self.write_buf[..len])
            .map_err(FramerError::Io)?;
        Ok(())
    }

    fn accept_len<E>(
        &mut self,
        websocket_context: &&WebSocketContext,
    ) -> Result<usize, FramerError<E>> {
        self.websocket
            .server_accept(
                &websocket_context.sec_websocket_key,
                None,
                &mut self.write_buf,
            )
            .map_err(FramerError::WebSocket)
    }
}

impl<'a, TRng, TWebSocketType> Framer<'a, TRng, TWebSocketType>
where
    TRng: RngCore,
    TWebSocketType: WebSocketType,
{
    // read and write buffers are usually quite small (4KB) and can be smaller
    // than the frame buffer but use whatever is is appropriate for your stream
    pub fn new(
        read_buf: &'a mut [u8],
        read_cursor: &'a mut usize,
        write_buf: &'a mut [u8],
        websocket: &'a mut WebSocket<TRng, TWebSocketType>,
    ) -> Self {
        Self {
            read_buf,
            write_buf,
            read_cursor,
            frame_cursor: 0,
            read_len: 0,
            websocket,
        }
    }

    pub fn state(&self) -> WebSocketState {
        self.websocket.state
    }

    fn close_len<E>(
        &mut self,
        close_status: WebSocketCloseStatusCode,
        status_description: Option<&str>,
    ) -> Result<usize, FramerError<E>> {
        self.websocket
            .close(close_status, status_description, self.write_buf)
            .map_err(FramerError::WebSocket)
    }

    // calling close on a websocket that has already been closed by the other party has no effect
    pub fn close<E>(
        &mut self,
        stream: &mut impl Write<E>,
        close_status: WebSocketCloseStatusCode,
        status_description: Option<&str>,
    ) -> Result<(), FramerError<E>> {
        let len = self.close_len(close_status, status_description)?;
        stream
            .write_all(&self.write_buf[..len])
            .map_err(FramerError::Io)?;
        Ok(())
    }

    fn write_len<E>(
        &mut self,
        message_type: WebSocketSendMessageType,
        end_of_message: bool,
        frame_buf: &[u8],
    ) -> Result<usize, FramerError<E>> {
        self.websocket
            .write(message_type, end_of_message, frame_buf, self.write_buf)
            .map_err(FramerError::WebSocket)
    }

    pub fn write<E>(
        &mut self,
        stream: &mut impl Write<E>,
        message_type: WebSocketSendMessageType,
        end_of_message: bool,
        frame_buf: &[u8],
    ) -> Result<(), FramerError<E>> {
        let len = self.write_len(message_type, end_of_message, frame_buf)?;
        stream
            .write_all(&self.write_buf[..len])
            .map_err(FramerError::Io)?;
        Ok(())
    }

    // frame_buf should be large enough to hold an entire websocket text frame
    // this function will block until it has recieved a full websocket frame.
    // It will wait until the last fragmented frame has arrived.
    pub fn read<'b, E>(
        &mut self,
        stream: &mut (impl Read<E> + Write<E>),
        frame_buf: &'b mut [u8],
    ) -> Result<ReadResult<'b>, FramerError<E>> {
        loop {
            if *self.read_cursor == 0 || *self.read_cursor == self.read_len {
                self.read_len = stream.read(self.read_buf).map_err(FramerError::Io)?;
                *self.read_cursor = 0;
            }

            if self.read_len == 0 {
                return Ok(ReadResult::Closed);
            }

            loop {
                if *self.read_cursor == self.read_len {
                    break;
                }

                if self.frame_cursor == frame_buf.len() {
                    return Err(FramerError::FrameTooLarge(frame_buf.len()));
                }

                let ws_result = self
                    .websocket
                    .read(
                        &self.read_buf[*self.read_cursor..self.read_len],
                        &mut frame_buf[self.frame_cursor..],
                    )
                    .map_err(FramerError::WebSocket)?;

                *self.read_cursor += ws_result.len_from;

                match ws_result.message_type {
                    WebSocketReceiveMessageType::Binary => {
                        self.frame_cursor += ws_result.len_to;
                        if ws_result.end_of_message {
                            let frame = &frame_buf[..self.frame_cursor];
                            self.frame_cursor = 0;
                            return Ok(ReadResult::Binary(frame));
                        }
                    }
                    WebSocketReceiveMessageType::Text => {
                        self.frame_cursor += ws_result.len_to;
                        if ws_result.end_of_message {
                            let frame = &frame_buf[..self.frame_cursor];
                            self.frame_cursor = 0;
                            let text = core::str::from_utf8(frame).map_err(FramerError::Utf8)?;
                            return Ok(ReadResult::Text(text));
                        }
                    }
                    WebSocketReceiveMessageType::CloseMustReply => {
                        self.send_back(
                            stream,
                            frame_buf,
                            ws_result.len_to,
                            WebSocketSendMessageType::CloseReply,
                        )?;
                        return Ok(ReadResult::Closed);
                    }
                    WebSocketReceiveMessageType::CloseCompleted => return Ok(ReadResult::Closed),
                    WebSocketReceiveMessageType::Ping => {
                        self.send_back(
                            stream,
                            frame_buf,
                            ws_result.len_to,
                            WebSocketSendMessageType::Pong,
                        )?;
                    }
                    WebSocketReceiveMessageType::Pong => {
                        let bytes =
                            &frame_buf[self.frame_cursor..self.frame_cursor + ws_result.len_to];
                        return Ok(ReadResult::Pong(bytes));
                    }
                }
            }
        }
    }

    fn send_back<E>(
        &mut self,
        stream: &mut impl Write<E>,
        frame_buf: &'_ mut [u8],
        len_to: usize,
        send_message_type: WebSocketSendMessageType,
    ) -> Result<(), FramerError<E>> {
        let len = self.send_back_data(&frame_buf, len_to, send_message_type)?;
        stream
            .write_all(&self.write_buf[..len])
            .map_err(FramerError::Io)?;
        Ok(())
    }

    fn send_back_data<E>(
        &mut self,
        frame_buf: &&mut [u8],
        len_to: usize,
        send_message_type: WebSocketSendMessageType,
    ) -> Result<usize, FramerError<E>> {
        let payload_len = min(self.write_buf.len(), len_to);
        let from = &frame_buf[self.frame_cursor..self.frame_cursor + payload_len];
        self.websocket
            .write(send_message_type, true, from, &mut self.write_buf)
            .map_err(FramerError::WebSocket)
    }
}

#[cfg(feature = "async")]
impl<'a, TRng> Framer<'a, TRng, crate::Client>
where
    TRng: RngCore,
{
    pub async fn connect_async<S: AsyncRead<E> + AsyncWrite<E> + Unpin, E>(
        &mut self,
        stream: &mut S,
        websocket_options: &'a WebSocketOptions<'a>,
    ) -> Result<Option<WebSocketSubProtocol>, FramerError<E>> {
        let (len, web_socket_key) = self
            .websocket
            .client_connect(websocket_options, &mut self.write_buf)
            .map_err(FramerError::WebSocket)?;
        stream
            .write_all(&self.write_buf[..len])
            .await
            .map_err(FramerError::Io)?;
        *self.read_cursor = 0;

        loop {
            // read the response from the server and check it to complete the opening handshake
            let received_size = stream
                .read(&mut self.read_buf[*self.read_cursor..])
                .await
                .map_err(FramerError::Io)?;

            match self.websocket.client_accept(
                &web_socket_key,
                &self.read_buf[..*self.read_cursor + received_size],
            ) {
                Ok((len, sub_protocol)) => {
                    // "consume" the HTTP header that we have read from the stream
                    // read_cursor would be 0 if we exactly read the HTTP header from the stream and nothing else
                    *self.read_cursor += received_size - len;
                    return Ok(sub_protocol);
                }
                Err(crate::Error::HttpHeaderIncomplete) => {
                    *self.read_cursor += received_size;
                    // continue reading HTTP header in loop
                }
                Err(e) => {
                    *self.read_cursor += received_size;
                    return Err(FramerError::WebSocket(e));
                }
            }
        }
    }
}

#[cfg(feature = "async")]
impl<'a, TRng> Framer<'a, TRng, crate::Server>
where
    TRng: RngCore,
{
    pub async fn accept_async<W: AsyncWrite<E> + Unpin, E>(
        &mut self,
        stream: &mut W,
        websocket_context: &WebSocketContext,
    ) -> Result<(), FramerError<E>> {
        let len = self.accept_len(&websocket_context)?;

        stream
            .write_all(&self.write_buf[..len])
            .await
            .map_err(FramerError::Io)?;
        Ok(())
    }
}

#[cfg(feature = "async")]
impl<'a, TRng, TWebSocketType> Framer<'a, TRng, TWebSocketType>
where
    TRng: RngCore,
    TWebSocketType: WebSocketType,
{
    // calling close on a websocket that has already been closed by the other party has no effect
    pub async fn close_async<W: AsyncWrite<E> + Unpin, E>(
        &mut self,
        stream: &mut W,
        close_status: WebSocketCloseStatusCode,
        status_description: Option<&str>,
    ) -> Result<(), FramerError<E>> {
        let len = self.close_len(close_status, status_description)?;
        stream
            .write_all(&self.write_buf[..len])
            .await
            .map_err(FramerError::Io)?;
        Ok(())
    }

    pub async fn write_async<W: AsyncWrite<E> + Unpin, E>(
        &mut self,
        stream: &mut W,
        message_type: WebSocketSendMessageType,
        end_of_message: bool,
        frame_buf: &[u8],
    ) -> Result<(), FramerError<E>> {
        let len = self.write_len(message_type, end_of_message, frame_buf)?;

        stream
            .write_all(&self.write_buf[..len])
            .await
            .map_err(FramerError::Io)?;
        Ok(())
    }

    pub async fn read_async<'b, S: AsyncWrite<E> + AsyncRead<E> + Unpin, E>(
        &mut self,
        stream: &mut S,
        frame_buf: &'b mut [u8],
    ) -> Result<ReadResult<'b>, FramerError<E>> {
        loop {
            if *self.read_cursor == 0 || *self.read_cursor == self.read_len {
                self.read_len = stream.read(self.read_buf).await.map_err(FramerError::Io)?;
                *self.read_cursor = 0;
            }

            if self.read_len == 0 {
                return Ok(ReadResult::Closed);
            }

            loop {
                if *self.read_cursor == self.read_len {
                    break;
                }

                if self.frame_cursor == frame_buf.len() {
                    return Err(FramerError::FrameTooLarge(frame_buf.len()));
                }

                let ws_result = self
                    .websocket
                    .read(
                        &self.read_buf[*self.read_cursor..self.read_len],
                        &mut frame_buf[self.frame_cursor..],
                    )
                    .map_err(FramerError::WebSocket)?;

                *self.read_cursor += ws_result.len_from;

                match ws_result.message_type {
                    WebSocketReceiveMessageType::Binary => {
                        self.frame_cursor += ws_result.len_to;
                        if ws_result.end_of_message {
                            let frame = &frame_buf[..self.frame_cursor];
                            self.frame_cursor = 0;
                            return Ok(ReadResult::Binary(frame));
                        }
                    }
                    WebSocketReceiveMessageType::Text => {
                        self.frame_cursor += ws_result.len_to;
                        if ws_result.end_of_message {
                            let frame = &frame_buf[..self.frame_cursor];
                            self.frame_cursor = 0;
                            let text = core::str::from_utf8(frame).map_err(FramerError::Utf8)?;
                            return Ok(ReadResult::Text(text));
                        }
                    }
                    WebSocketReceiveMessageType::CloseMustReply => {
                        self.send_back_async(
                            stream,
                            frame_buf,
                            ws_result.len_to,
                            WebSocketSendMessageType::CloseReply,
                        )
                        .await?;
                        return Ok(ReadResult::Closed);
                    }
                    WebSocketReceiveMessageType::CloseCompleted => return Ok(ReadResult::Closed),
                    WebSocketReceiveMessageType::Ping => {
                        self.send_back_async(
                            stream,
                            frame_buf,
                            ws_result.len_to,
                            WebSocketSendMessageType::Pong,
                        )
                        .await?;
                    }
                    WebSocketReceiveMessageType::Pong => {
                        let bytes =
                            &frame_buf[self.frame_cursor..self.frame_cursor + ws_result.len_to];
                        return Ok(ReadResult::Pong(bytes));
                    }
                }
            }
        }
    }

    async fn send_back_async<W: AsyncWrite<E> + Unpin, E>(
        &mut self,
        stream: &mut W,
        frame_buf: &'_ mut [u8],
        len_to: usize,
        send_message_type: WebSocketSendMessageType,
    ) -> Result<(), FramerError<E>> {
        let len = self.send_back_data(&frame_buf, len_to, send_message_type)?;
        stream
            .write_all(&self.write_buf[..len])
            .await
            .map_err(FramerError::Io)?;
        Ok(())
    }
}
