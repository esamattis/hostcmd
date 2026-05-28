use anyhow::{Context, Result, bail};
use bytes::{Buf, BufMut};

/// Maximum number of streamed bytes sent in one websocket frame.
pub const CHUNK_SIZE: usize = 16 * 1024;

/// Number of bytes used by a tagged length-prefixed byte frame header.
pub const BYTE_FRAME_HEADER_SIZE: usize = 5;

/// Wire-level tag values used for `ClientFrame` payloads.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientFrameTag {
    /// Tag byte for `ClientFrame::Exec`.
    Exec = 1,
    /// Tag byte for `ClientFrame::Cancel`.
    Cancel = 2,
    /// Tag byte for `ClientFrame::Stdin`.
    Stdin = 3,
    /// Tag byte for `ClientFrame::StdinEof`.
    StdinEof = 4,
}

impl TryFrom<u8> for ClientFrameTag {
    type Error = anyhow::Error;

    /// Converts a raw client frame tag byte into its typed representation.
    fn try_from(value: u8) -> Result<Self, anyhow::Error> {
        match value {
            value if value == Self::Exec as u8 => Ok(Self::Exec),
            value if value == Self::Cancel as u8 => Ok(Self::Cancel),
            value if value == Self::Stdin as u8 => Ok(Self::Stdin),
            value if value == Self::StdinEof as u8 => Ok(Self::StdinEof),
            tag => bail!("unknown client frame {tag}"),
        }
    }
}

/// Wire-level tag values used for server-to-client payloads.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServerFrameTag {
    /// Tag byte for a stdout payload frame.
    Stdout = 1,
    /// Tag byte for a stderr payload frame.
    Stderr = 2,
    /// Tag byte for a command exit frame.
    Exit = 3,
    /// Tag byte for an execution error frame.
    Error = 4,
}

impl ServerFrameTag {
    /// Returns the wire tag byte for this server frame tag.
    fn as_u8(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for ServerFrameTag {
    type Error = anyhow::Error;

    /// Converts a raw server frame tag byte into its typed representation.
    fn try_from(value: u8) -> Result<Self, anyhow::Error> {
        match value {
            value if value == Self::Stdout as u8 => Ok(Self::Stdout),
            value if value == Self::Stderr as u8 => Ok(Self::Stderr),
            value if value == Self::Exit as u8 => Ok(Self::Exit),
            value if value == Self::Error as u8 => Ok(Self::Error),
            tag => bail!("unknown server frame {tag}"),
        }
    }
}

/// One local environment variable forwarded from the client to the server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForwardedEnvVar {
    /// Name of the environment variable to expose to the spawned command.
    pub name: String,
    /// Value copied from the client environment for this variable.
    pub value: String,
}

/// Binary protocol frames sent from client to server.
#[derive(Debug, Eq, PartialEq)]
pub enum ClientFrame {
    /// Execute a command after websocket upgrade authentication has succeeded.
    Exec {
        /// Program and arguments to run on the server.
        command: Vec<String>,
        /// Local environment variables forwarded from the client.
        forward_env: Vec<ForwardedEnvVar>,
        /// Hostname of the client machine initiating the command.
        client_hostname: String,
        /// Username of the client user initiating the command.
        client_username: String,
        /// Current working directory of the client process initiating the command.
        client_cwd: String,
    },
    /// Request graceful cancellation of the running command.
    Cancel,
    /// A bounded chunk of bytes to write to the running command's stdin.
    Stdin(Vec<u8>),
    /// Signal that no more stdin bytes will be sent for the running command.
    StdinEof,
}

impl ClientFrame {
    /// Encodes this client protocol frame as binary websocket payload bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            ClientFrame::Exec {
                command,
                forward_env,
                client_hostname,
                client_username,
                client_cwd,
            } => {
                out.put_u8(ClientFrameTag::Exec as u8);
                out.put_u32(command.len() as u32);
                for arg in command {
                    put_string(&mut out, arg);
                }
                out.put_u32(forward_env.len() as u32);
                for env_var in forward_env {
                    put_string(&mut out, &env_var.name);
                    put_string(&mut out, &env_var.value);
                }
                put_string(&mut out, client_hostname);
                put_string(&mut out, client_username);
                put_string(&mut out, client_cwd);
            }
            ClientFrame::Cancel => out.put_u8(ClientFrameTag::Cancel as u8),
            ClientFrame::Stdin(bytes) => {
                out.put_u8(ClientFrameTag::Stdin as u8);
                put_bytes(&mut out, bytes);
            }
            ClientFrame::StdinEof => out.put_u8(ClientFrameTag::StdinEof as u8),
        }
        out
    }

    /// Decodes a binary websocket payload into a client protocol frame.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(bytes);
        let tag = ClientFrameTag::try_from(decoder.u8().context("reading client frame tag")?)
            .context("decoding client frame tag")?;

        match tag {
            ClientFrameTag::Exec => {
                let len = decoder
                    .u32()
                    .context("client exec: reading command argument count")?
                    as usize;
                let mut command = Vec::with_capacity(len);

                for index in 0..len {
                    command.push(decoder.string().with_context(|| {
                        format!("client exec: reading command argument {index}")
                    })?);
                }

                let forward_env_len = decoder
                    .u32()
                    .context("client exec: reading forwarded env count")?
                    as usize;
                let mut forward_env = Vec::with_capacity(forward_env_len);

                for index in 0..forward_env_len {
                    let name = decoder.string().with_context(|| {
                        format!("client exec: reading forwarded env {index} name")
                    })?;
                    let value = decoder.string().with_context(|| {
                        format!("client exec: reading forwarded env {index} value")
                    })?;
                    forward_env.push(ForwardedEnvVar { name, value });
                }

                let client_hostname = decoder.string().context("client exec: reading hostname")?;
                let client_username = decoder.string().context("client exec: reading username")?;
                let client_cwd = decoder.string().context("client exec: reading cwd")?;

                decoder
                    .finish()
                    .context("client exec: validating trailing bytes")?;
                Ok(ClientFrame::Exec {
                    command,
                    forward_env,
                    client_hostname,
                    client_username,
                    client_cwd,
                })
            }
            ClientFrameTag::Cancel => {
                decoder
                    .finish()
                    .context("client cancel: validating trailing bytes")?;
                Ok(ClientFrame::Cancel)
            }
            ClientFrameTag::Stdin => {
                let bytes = decoder
                    .bytes()
                    .context("client stdin: reading bytes")?
                    .to_vec();
                decoder
                    .finish()
                    .context("client stdin: validating trailing bytes")?;
                Ok(ClientFrame::Stdin(bytes))
            }
            ClientFrameTag::StdinEof => {
                decoder
                    .finish()
                    .context("client stdin_eof: validating trailing bytes")?;
                Ok(ClientFrame::StdinEof)
            }
        }
    }
}

/// Server-to-client control frames that are still encoded through structured variants.
#[derive(Debug, Eq, PartialEq)]
pub enum ServerControlFrame {
    /// Final process exit code.
    Exit(i32),
    /// Error that prevented successful execution.
    Error(String),
}

/// Decoded server-to-client frames with borrowed output payloads.
#[derive(Debug, Eq, PartialEq)]
pub enum DecodedServerFrame<'a> {
    /// A borrowed bounded chunk of stdout bytes.
    Stdout(&'a [u8]),
    /// A borrowed bounded chunk of stderr bytes.
    Stderr(&'a [u8]),
    /// Final process exit code.
    Exit(i32),
    /// Error that prevented successful execution.
    Error(String),
}

impl ServerControlFrame {
    /// Encodes this server control frame as binary websocket payload bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            ServerControlFrame::Exit(code) => {
                out.put_u8(ServerFrameTag::Exit as u8);
                out.put_i32(*code);
            }
            ServerControlFrame::Error(message) => {
                out.put_u8(ServerFrameTag::Error as u8);
                put_string(&mut out, message);
            }
        }
        out
    }
}

impl<'a> DecodedServerFrame<'a> {
    /// Decodes a binary websocket payload into a server-to-client protocol frame.
    pub fn decode(bytes: &'a [u8]) -> Result<Self> {
        let mut decoder = Decoder::new(bytes);
        let tag = ServerFrameTag::try_from(decoder.u8().context("reading server frame tag")?)
            .context("decoding server frame tag")?;

        match tag {
            ServerFrameTag::Stdout => {
                let bytes = decoder.bytes().context("server stdout: reading bytes")?;
                decoder
                    .finish()
                    .context("server stdout: validating trailing bytes")?;
                Ok(DecodedServerFrame::Stdout(bytes))
            }
            ServerFrameTag::Stderr => {
                let bytes = decoder.bytes().context("server stderr: reading bytes")?;
                decoder
                    .finish()
                    .context("server stderr: validating trailing bytes")?;
                Ok(DecodedServerFrame::Stderr(bytes))
            }
            ServerFrameTag::Exit => {
                let code = decoder.i32().context("server exit: reading exit code")?;
                decoder
                    .finish()
                    .context("server exit: validating trailing bytes")?;
                Ok(DecodedServerFrame::Exit(code))
            }
            ServerFrameTag::Error => {
                let message = decoder.string().context("server error: reading message")?;
                decoder
                    .finish()
                    .context("server error: validating trailing bytes")?;
                Ok(DecodedServerFrame::Error(message))
            }
        }
    }
}

/// Writes a stdout byte-frame header into a preallocated output buffer.
pub fn put_stdout_frame_header(out: &mut [u8], payload_len: usize) {
    put_byte_frame_header(out, ServerFrameTag::Stdout, payload_len);
}

/// Writes a stderr byte-frame header into a preallocated output buffer.
pub fn put_stderr_frame_header(out: &mut [u8], payload_len: usize) {
    put_byte_frame_header(out, ServerFrameTag::Stderr, payload_len);
}

/// Writes a tagged length-prefixed byte-frame header into an output buffer.
fn put_byte_frame_header(out: &mut [u8], tag: ServerFrameTag, payload_len: usize) {
    assert!(
        out.len() >= BYTE_FRAME_HEADER_SIZE,
        "byte frame header destination should have enough capacity"
    );
    assert!(
        u32::try_from(payload_len).is_ok(),
        "byte frame payload length should fit in the wire format"
    );

    out[0] = tag.as_u8();
    out[1..BYTE_FRAME_HEADER_SIZE].copy_from_slice(&(payload_len as u32).to_be_bytes());
}

/// Appends a length-prefixed UTF-8 string to a frame buffer.
fn put_string(out: &mut Vec<u8>, value: &str) {
    put_bytes(out, value.as_bytes());
}

/// Appends length-prefixed bytes to a frame buffer.
fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    out.put_u32(value.len() as u32);
    out.put_slice(value);
}

/// Cursor-based decoder for length-prefixed protocol frames.
struct Decoder<'a> {
    /// Remaining frame payload being decoded.
    bytes: &'a [u8],
}

impl<'a> Decoder<'a> {
    /// Creates a decoder over one complete frame payload.
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Reads a single unsigned byte from the frame.
    fn u8(&mut self) -> Result<u8> {
        if !self.bytes.has_remaining() {
            bail!("truncated frame")
        }

        Ok(self.bytes.get_u8())
    }

    /// Reads a big-endian unsigned integer from the frame.
    fn u32(&mut self) -> Result<u32> {
        if self.bytes.remaining() < 4 {
            bail!("truncated frame")
        }

        Ok(self.bytes.get_u32())
    }

    /// Reads a big-endian signed integer from the frame.
    fn i32(&mut self) -> Result<i32> {
        if self.bytes.remaining() < 4 {
            bail!("truncated frame")
        }

        Ok(self.bytes.get_i32())
    }

    /// Reads length-prefixed bytes from the frame.
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    /// Reads a length-prefixed UTF-8 string from the frame.
    fn string(&mut self) -> Result<String> {
        let bytes = self.bytes()?;
        String::from_utf8(bytes.to_vec()).context("invalid utf-8 string")
    }

    /// Takes a fixed number of bytes from the current frame offset.
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.bytes.remaining() < len {
            bail!("truncated frame")
        }

        let (bytes, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(bytes)
    }

    /// Verifies that all frame bytes were consumed.
    fn finish(&self) -> Result<()> {
        if self.bytes.remaining() == 0 {
            Ok(())
        } else {
            bail!("trailing frame bytes")
        }
    }
}

/// Unit tests for protocol frame encoding and decoding.
#[cfg(test)]
mod tests {
    use super::{
        BYTE_FRAME_HEADER_SIZE, ClientFrame, DecodedServerFrame, ForwardedEnvVar,
        ServerControlFrame, put_stdout_frame_header,
    };

    /// Formats an error with its full anyhow context chain.
    fn error_chain(error: anyhow::Error) -> String {
        format!("{error:#}")
    }

    /// Verifies that client exec frames round-trip through the wire format.
    #[test]
    fn client_exec_round_trip() {
        let frame = ClientFrame::Exec {
            command: vec!["sh".into(), "-lc".into(), "printf test".into()],
            forward_env: vec![
                ForwardedEnvVar {
                    name: "DISPLAY".into(),
                    value: ":0".into(),
                },
                ForwardedEnvVar {
                    name: "SSH_AUTH_SOCK".into(),
                    value: "/tmp/ssh.sock".into(),
                },
            ],
            client_hostname: "workstation".into(),
            client_username: "esamatti".into(),
            client_cwd: "/workspace/project".into(),
        };

        let decoded = ClientFrame::decode(&frame.encode()).expect("client exec should decode");

        assert_eq!(
            decoded, frame,
            "client exec frame should round-trip through encode and decode"
        );
    }

    /// Verifies that server error frames round-trip through the wire format.
    #[test]
    fn server_error_round_trip() {
        let frame = ServerControlFrame::Error("permission denied".into());

        let encoded = frame.encode();
        let decoded = DecodedServerFrame::decode(&encoded).expect("server error should decode");

        assert_eq!(
            decoded,
            DecodedServerFrame::Error("permission denied".into()),
            "server error frame should round-trip through encode and decode"
        );
    }

    /// Verifies that borrowed server output decoding avoids allocating output payloads.
    #[test]
    fn borrowed_server_stdout_decodes_payload_slice() {
        let mut encoded = vec![0; BYTE_FRAME_HEADER_SIZE + 3];
        put_stdout_frame_header(&mut encoded[..BYTE_FRAME_HEADER_SIZE], 3);
        encoded[BYTE_FRAME_HEADER_SIZE..].copy_from_slice(b"abc");

        let decoded = DecodedServerFrame::decode(&encoded).expect("borrowed stdout should decode");

        assert_eq!(
            decoded,
            DecodedServerFrame::Stdout(b"abc"),
            "borrowed stdout frame should reference the expected payload bytes"
        );
    }

    /// Verifies that a hand-written client exec frame decodes successfully.
    #[test]
    fn client_exec_decodes_from_inlined_frame() {
        let encoded = [
            1, // client exec tag
            0, 0, 0, 2, // command length
            0, 0, 0, 2, b's', b'h', // command[0]
            0, 0, 0, 5, b'e', b'c', b'h', b'o', b'o', // command[1]
            0, 0, 0, 0, // forwarded env length
            0, 0, 0, 5, b'h', b'o', b's', b't', b'a', // hostname
            0, 0, 0, 5, b'u', b's', b'e', b'r', b'a', // username
            0, 0, 0, 5, b'/', b't', b'm', b'p', b'a', // cwd
        ];

        let decoded = ClientFrame::decode(&encoded).expect("inlined client exec should decode");

        assert_eq!(
            decoded,
            ClientFrame::Exec {
                command: vec!["sh".into(), "echoo".into()],
                forward_env: vec![],
                client_hostname: "hosta".into(),
                client_username: "usera".into(),
                client_cwd: "/tmpa".into(),
            },
            "inlined client exec bytes should decode into the expected frame"
        );
    }

    /// Verifies that truncated client exec payloads include the exec tag prefix in context.
    #[test]
    fn client_exec_decode_error_includes_tag_prefix() {
        let encoded = [1, 0, 0, 0, 1];
        let error = ClientFrame::decode(&encoded).expect_err("client exec should fail to decode");
        let message = error_chain(error);

        assert!(
            message.contains("client exec: reading command argument 0"),
            "client exec decode errors should mention the argument index context"
        );
        assert!(
            message.contains("truncated frame"),
            "client exec decode errors should preserve the truncated frame root cause"
        );
    }

    /// Verifies that trailing bytes in client cancel frames include the cancel tag prefix.
    #[test]
    fn client_cancel_trailing_bytes_error_includes_tag_prefix() {
        let encoded = [2, 0];
        let error = ClientFrame::decode(&encoded).expect_err("client cancel should fail to decode");
        let message = error_chain(error);

        assert!(
            message.contains("client cancel: validating trailing bytes"),
            "client cancel decode errors should include the cancel tag context"
        );
        assert!(
            message.contains("trailing frame bytes"),
            "client cancel decode errors should preserve the trailing bytes root cause"
        );
    }

    /// Verifies that truncated server exit payloads include the exit tag prefix in context.
    #[test]
    fn server_exit_decode_error_includes_tag_prefix() {
        let encoded = [3];
        let error =
            DecodedServerFrame::decode(&encoded).expect_err("server exit should fail to decode");
        let message = error_chain(error);

        assert!(
            message.contains("server exit: reading exit code"),
            "server exit decode errors should include the exit tag context"
        );
        assert!(
            message.contains("truncated frame"),
            "server exit decode errors should preserve the truncated frame root cause"
        );
    }

    /// Verifies that invalid UTF-8 in server error payloads includes the error tag prefix.
    #[test]
    fn server_error_decode_error_includes_tag_prefix() {
        let encoded = [4, 0, 0, 0, 1, 0xff];
        let error =
            DecodedServerFrame::decode(&encoded).expect_err("server error should fail to decode");
        let message = error_chain(error);

        assert!(
            message.contains("server error: reading message"),
            "server error decode errors should include the error tag context"
        );
        assert!(
            message.contains("invalid utf-8 string"),
            "server error decode errors should preserve the invalid UTF-8 root cause"
        );
    }
}
