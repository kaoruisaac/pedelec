use serde_json::Value;
use std::fmt;
use std::io::Read;
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonLineError {
    InvalidMaxFrameSize,
    InvalidUtf8,
    MalformedJson { message: String },
    FrameTooLarge { max_bytes: usize },
    UncleanEof { partial_bytes: usize },
    Io { stream: &'static str, error: String },
}

impl fmt::Display for JsonLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMaxFrameSize => {
                write!(f, "JSONL maximum frame size must be greater than zero")
            }
            Self::InvalidUtf8 => write!(f, "JSONL frame was not valid UTF-8"),
            Self::MalformedJson { message } => write!(f, "malformed JSONL frame: {message}"),
            Self::FrameTooLarge { max_bytes } => {
                write!(f, "JSONL frame exceeded maximum size of {max_bytes} bytes")
            }
            Self::UncleanEof { partial_bytes } => {
                write!(
                    f,
                    "JSONL stdout ended with {partial_bytes} unterminated bytes"
                )
            }
            Self::Io { stream, error } => write!(f, "JSONL {stream} read failed: {error}"),
        }
    }
}

impl std::error::Error for JsonLineError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonLineEof {
    Clean,
    Unclean { partial_bytes: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonLineEvent {
    StdoutFrame(Value),
    StderrChunk(String),
    StdoutEof { clean: bool },
    StderrEof,
    Error(JsonLineError),
}

/// Incremental newline-delimited JSON decoder. It accepts arbitrary byte
/// chunks, including chunks that split a UTF-8 code point.
#[derive(Debug, Clone)]
pub struct JsonLineFramer {
    buffer: Vec<u8>,
    max_frame_bytes: usize,
}

impl JsonLineFramer {
    pub fn new(max_frame_bytes: usize) -> Result<Self, JsonLineError> {
        if max_frame_bytes == 0 {
            return Err(JsonLineError::InvalidMaxFrameSize);
        }
        Ok(Self {
            buffer: Vec::new(),
            max_frame_bytes,
        })
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, JsonLineError> {
        let mut frames = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                frames.push(self.decode_buffer()?);
                self.buffer.clear();
            } else {
                if self.buffer.len() >= self.max_frame_bytes {
                    return Err(JsonLineError::FrameTooLarge {
                        max_bytes: self.max_frame_bytes,
                    });
                }
                self.buffer.push(*byte);
            }
        }
        Ok(frames)
    }

    pub fn finish(&self) -> JsonLineEof {
        if self.buffer.is_empty() {
            JsonLineEof::Clean
        } else {
            JsonLineEof::Unclean {
                partial_bytes: self.buffer.len(),
            }
        }
    }

    fn decode_buffer(&self) -> Result<Value, JsonLineError> {
        let line = if self.buffer.last() == Some(&b'\r') {
            &self.buffer[..self.buffer.len() - 1]
        } else {
            &self.buffer
        };
        let text = std::str::from_utf8(line).map_err(|_| JsonLineError::InvalidUtf8)?;
        serde_json::from_str(text).map_err(|error| JsonLineError::MalformedJson {
            message: error.to_string(),
        })
    }
}

/// A pair of reader threads that keeps stdout protocol frames and stderr
/// diagnostics on separate event variants.
#[derive(Debug)]
pub struct JsonLineChannel {
    events: Receiver<JsonLineEvent>,
}

impl JsonLineChannel {
    pub fn spawn<R1, R2>(
        stdout: R1,
        stderr: R2,
        max_frame_bytes: usize,
    ) -> Result<Self, JsonLineError>
    where
        R1: Read + Send + 'static,
        R2: Read + Send + 'static,
    {
        JsonLineFramer::new(max_frame_bytes)?;
        let (tx, events) = mpsc::sync_channel(128);
        spawn_stdout_reader(stdout, tx.clone(), max_frame_bytes);
        spawn_stderr_reader(stderr, tx, max_frame_bytes);
        Ok(Self { events })
    }

    pub fn recv(&self) -> Result<JsonLineEvent, RecvError> {
        self.events.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<JsonLineEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn try_recv(&self) -> Result<JsonLineEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }

    pub fn into_receiver(self) -> Receiver<JsonLineEvent> {
        self.events
    }
}

fn spawn_stdout_reader<R: Read + Send + 'static>(
    mut reader: R,
    tx: SyncSender<JsonLineEvent>,
    max_frame_bytes: usize,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("pedelec-runtime-stdout".to_string())
        .spawn(move || {
            let mut framer = JsonLineFramer::new(max_frame_bytes).expect("validated frame size");
            let mut chunk = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => {
                        if let JsonLineEof::Unclean { partial_bytes } = framer.finish() {
                            let _ = tx.send(JsonLineEvent::Error(JsonLineError::UncleanEof {
                                partial_bytes,
                            }));
                            let _ = tx.send(JsonLineEvent::StdoutEof { clean: false });
                        } else {
                            let _ = tx.send(JsonLineEvent::StdoutEof { clean: true });
                        }
                        break;
                    }
                    Ok(read) => match framer.push(&chunk[..read]) {
                        Ok(frames) => {
                            for frame in frames {
                                if tx.send(JsonLineEvent::StdoutFrame(frame)).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = tx.send(JsonLineEvent::Error(error));
                            break;
                        }
                    },
                    Err(error) => {
                        let _ = tx.send(JsonLineEvent::Error(JsonLineError::Io {
                            stream: "stdout",
                            error: error.to_string(),
                        }));
                        break;
                    }
                }
            }
        })
        .expect("could not start JSONL stdout reader")
}

fn spawn_stderr_reader<R: Read + Send + 'static>(
    mut reader: R,
    tx: SyncSender<JsonLineEvent>,
    _max_frame_bytes: usize,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("pedelec-runtime-stderr".to_string())
        .spawn(move || {
            let mut chunk = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => {
                        let _ = tx.send(JsonLineEvent::StderrEof);
                        break;
                    }
                    Ok(read) => {
                        let text = String::from_utf8_lossy(&chunk[..read]).into_owned();
                        if tx.send(JsonLineEvent::StderrChunk(text)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(JsonLineEvent::Error(JsonLineError::Io {
                            stream: "stderr",
                            error: error.to_string(),
                        }));
                        break;
                    }
                }
            }
        })
        .expect("could not start JSONL stderr reader")
}
