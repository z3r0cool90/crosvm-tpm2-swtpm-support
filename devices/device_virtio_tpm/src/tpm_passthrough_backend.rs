// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use base::error;
use base::AsRawDescriptor;
use base::RawDescriptor;

use crate::TpmBackend;

/// Backend that passes TPM commands directly to a host TPM device (e.g. /dev/tpm0).
pub struct TpmPassthroughBackend {
    device: File,
    response_buffer: Vec<u8>,
}

impl TpmPassthroughBackend {
    pub fn new<P: AsRef<Path>>(device_path: P) -> Result<Self> {
        let device = File::options()
            .read(true)
            .write(true)
            .open(device_path.as_ref())
            .with_context(|| {
                format!(
                    "failed to open TPM device at {}",
                    device_path.as_ref().display()
                )
            })?;

        Ok(Self {
            device,
            response_buffer: vec![0u8; 4096],
        })
    }
}

impl TpmBackend for TpmPassthroughBackend {
    fn execute_command<'a>(&'a mut self, command: &[u8]) -> &'a [u8] {
        // A TPM character device treats one write as one complete command.  Do not retry a
        // short write: the remainder would be interpreted as a second command.
        match self.device.write(command) {
            Ok(n) if n == command.len() => {}
            Ok(n) => {
                error!(
                    "tpm passthrough short write: {} of {} bytes",
                    n,
                    command.len()
                );
                return &[];
            }
            Err(e) => {
                error!("tpm passthrough write error: {}", e);
                return &[];
            }
        }

        // The device returns one complete response per read.  Reading the header on its own
        // discards the body, so read into the full buffer and validate the embedded length.
        let read_size = match self.device.read(&mut self.response_buffer) {
            Ok(n) => n,
            Err(e) => {
                error!("tpm passthrough read error: {}", e);
                return &[];
            }
        };

        if read_size < 10 {
            error!("tpm passthrough truncated response: {} bytes", read_size);
            return &[];
        }

        let response_size = u32::from_be_bytes([
            self.response_buffer[2],
            self.response_buffer[3],
            self.response_buffer[4],
            self.response_buffer[5],
        ]) as usize;

        if response_size != read_size {
            error!(
                "tpm passthrough length mismatch: header says {}, read {}",
                response_size, read_size
            );
            return &[];
        }

        &self.response_buffer[..read_size]
    }

    fn keep_rds(&self) -> Vec<RawDescriptor> {
        vec![self.device.as_raw_descriptor()]
    }
}
