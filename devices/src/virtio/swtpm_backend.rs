// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use base::error;

use super::tpm::TpmBackend;

/// Backend that communicates with swtpm via Unix socket
pub struct SwtpmBackend {
    socket: UnixStream,
    response_buffer: Vec<u8>,
}

impl SwtpmBackend {
    pub fn new<P: AsRef<Path>>(socket_path: P) -> Result<Self> {
        let socket = UnixStream::connect(socket_path.as_ref()).with_context(|| {
            format!(
                "failed to connect to swtpm socket at {}",
                socket_path.as_ref().display()
            )
        })?;

        Ok(Self {
            socket,
            response_buffer: vec![0u8; 4096],
        })
    }
}

impl TpmBackend for SwtpmBackend {
    fn execute_command<'a>(&'a mut self, command: &[u8]) -> &'a [u8] {
        // Send command to swtpm
        if let Err(e) = self.socket.write_all(command) {
            error!("swtpm write error: {}", e);
            return &[];
        }

        // Read TPM response header (10 bytes) to get response size
        let mut header = [0u8; 10];
        if let Err(e) = self.socket.read_exact(&mut header) {
            error!("swtpm read header error: {}", e);
            return &[];
        }

        // Response size is at bytes 2-5 (big endian u32)
        let response_size =
            u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;

        // Ensure buffer is large enough
        if response_size > self.response_buffer.len() {
            self.response_buffer.resize(response_size, 0);
        }

        // Copy header to buffer
        self.response_buffer[..10].copy_from_slice(&header);

        // Read rest of response if any
        if response_size > 10 {
            if let Err(e) = self
                .socket
                .read_exact(&mut self.response_buffer[10..response_size])
            {
                error!("swtpm read body error: {}", e);
                return &[];
            }
        }

        &self.response_buffer[..response_size]
    }
}
