//! A blocking client of the control socket, for the `jackstay` CLI and the daemon's tests.

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use jackstay_proto::control::{self, Request, Response, VERSION};
use jackstay_proto::{DecodeError, LimitError};

#[derive(Debug)]
pub enum ClientError {
    /// The daemon is not running, or its socket cannot be reached.
    Connect(io::Error),
    Io(io::Error),
    /// The daemon closed the connection.
    Closed,
    /// The daemon sent something this client cannot read.
    Protocol(DecodeError),
    /// The daemon speaks another version.
    Version,
    /// The daemon refused the greeting.
    Refused,
    /// The request breaks a limit (a name too long), so it was not sent.
    Request(LimitError),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Connect(e) => write!(f, "cannot reach jackstayd: {e}"),
            ClientError::Io(e) => write!(f, "talking to jackstayd: {e}"),
            ClientError::Closed => write!(f, "jackstayd closed the connection"),
            ClientError::Protocol(e) => write!(f, "jackstayd sent a bad response: {e}"),
            ClientError::Version => write!(f, "jackstayd speaks another protocol version"),
            ClientError::Refused => write!(f, "jackstayd refused the connection"),
            ClientError::Request(e) => write!(f, "the request cannot be sent: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    input: Vec<u8>,
}

impl Client {
    /// Connects and says hello.
    ///
    /// # Errors
    /// A [`ClientError`] if the daemon cannot be reached or speaks another version.
    pub fn connect(socket: &Path, timeout: Duration) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).map_err(ClientError::Connect)?;
        stream.set_read_timeout(Some(timeout)).map_err(ClientError::Io)?;
        stream.set_write_timeout(Some(timeout)).map_err(ClientError::Io)?;
        let mut client = Client { stream, input: Vec::new() };
        match client.ask(&Request::Hello { version: VERSION })? {
            Response::Welcome { version } if version == VERSION => Ok(client),
            Response::Error(control::Error::UnsupportedVersion) => Err(ClientError::Version),
            _ => Err(ClientError::Refused),
        }
    }

    /// Sends `request` and waits for its response; `Changed` events of a subscription that arrive
    /// meanwhile are not responses, and are skipped.
    ///
    /// # Errors
    /// A [`ClientError`] if the exchange fails.
    pub fn ask(&mut self, request: &Request) -> Result<Response, ClientError> {
        self.send(request)?;
        loop {
            match self.receive()? {
                Response::Changed => {}
                response => return Ok(response),
            }
        }
    }

    /// Sends `request` without waiting: for a flow such as pairing, whose events and responses the
    /// caller reads with [`Client::receive`].
    ///
    /// # Errors
    /// A [`ClientError`] if the request breaks a limit or cannot be written.
    pub fn send(&mut self, request: &Request) -> Result<(), ClientError> {
        let mut out = Vec::new();
        control::encode_request(request, &mut out).map_err(ClientError::Request)?;
        self.stream.write_all(&out).map_err(ClientError::Io)
    }

    /// How long reads and writes may wait from now on (pairing waits for a person).
    ///
    /// # Errors
    /// The socket refused the timeout.
    pub fn set_timeout(&mut self, timeout: Duration) -> Result<(), ClientError> {
        self.stream.set_read_timeout(Some(timeout)).map_err(ClientError::Io)?;
        self.stream.set_write_timeout(Some(timeout)).map_err(ClientError::Io)
    }

    /// Waits for the next response (after `Subscribe`, the next `Changed`).
    ///
    /// # Errors
    /// A [`ClientError`] if the connection fails or the timeout passes.
    pub fn receive(&mut self) -> Result<Response, ClientError> {
        loop {
            if let Some((response, used)) =
                control::decode_response(&self.input).map_err(ClientError::Protocol)?
            {
                self.input.drain(..used);
                return Ok(response);
            }
            let mut buf = [0u8; 16 << 10];
            let n = self.stream.read(&mut buf).map_err(ClientError::Io)?;
            if n == 0 {
                return Err(ClientError::Closed);
            }
            self.input.extend_from_slice(buf.get(..n).unwrap_or_default());
        }
    }
}
