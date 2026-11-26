// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Acceptance CA for uploading and probing one or two dynamic TAs.

use std::{env, fs, io, process};

use libtrusty_rs::TipcChannel;
use sha2::{Digest, Sha256};

const SERVICE: &str = "com.android.trusty.apploader";
const PROBE_SERVICE: &str = "com.xkernel.test.dynamic-ta";
const BULK_PROBE_SERVICE: &str = "com.xkernel.test.dynamic-ta-bulk";
const VERSION: u32 = 1;
const CMD_BEGIN: u32 = 0;
const CMD_DATA: u32 = 2;
const CMD_COMMIT: u32 = 4;
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
// Keep upload and bulk messages within TIPC's 8 KiB receive-slot bound.
const DATA_BYTES: usize = 4 * 1024;
const BULK_CHUNK_BYTES: usize = 4 * 1024;
const BULK_TOTAL_BYTES: usize = 1024 * 1024;

fn main() {
    if let Err(error) = run() {
        eprintln!("ca-apploader: {error}");
        process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let first_image_path = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: ca-apploader <first-ta-elf> [--second <second-ta-elf>] [guest-cid]",
        )
    })?;
    let second_or_cid = args.next();
    let (second_image_path, cid) = if second_or_cid.as_deref() == Some("--second") {
        let second_image_path = args.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "--second requires a TA image path",
            )
        })?;
        (
            Some(second_image_path),
            args.next().unwrap_or_else(|| "103".into()),
        )
    } else {
        (None, second_or_cid.unwrap_or_else(|| "103".into()))
    };
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many arguments",
        ));
    }
    upload_image(&first_image_path, &cid)?;
    if let Some(second_image_path) = second_image_path {
        upload_image(&second_image_path, &cid)?;
        probe_ping(&cid)?;
        probe_bulk(&cid)?;
        probe_ping(&cid)?;
    } else {
        probe_ping(&cid)?;
    }
    Ok(())
}

fn upload_image(image_path: &str, cid: &str) -> io::Result<()> {
    let image = fs::read(image_path)?;
    if image.is_empty() || image.len() > MAX_IMAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TA image must be between 1 byte and 16 MiB",
        ));
    }
    let mut channel = TipcChannel::connect(&format!("VSOCK:{cid}:0"), SERVICE)
        .map_err(|error| io::Error::new(io::ErrorKind::ConnectionRefused, error))?;

    let mut begin = [0; 48];
    begin[..4].copy_from_slice(&CMD_BEGIN.to_le_bytes());
    begin[4..8].copy_from_slice(&VERSION.to_le_bytes());
    begin[8..16].copy_from_slice(&(image.len() as u64).to_le_bytes());
    begin[16..].copy_from_slice(&Sha256::digest(&image));
    channel.send(&begin)?;
    expect_status(&mut channel, CMD_BEGIN)?;

    let mut data = vec![0; 12 + DATA_BYTES];
    data[..4].copy_from_slice(&CMD_DATA.to_le_bytes());
    for (chunk_index, chunk) in image.chunks(DATA_BYTES).enumerate() {
        let offset = chunk_index * DATA_BYTES;
        data[4..12].copy_from_slice(&(offset as u64).to_le_bytes());
        data[12..12 + chunk.len()].copy_from_slice(chunk);
        channel.send(&data[..12 + chunk.len()])?;
        expect_status(&mut channel, CMD_DATA)?;
    }
    channel.send(&CMD_COMMIT.to_le_bytes())?;
    expect_status(&mut channel, CMD_COMMIT)?;
    println!(
        "ca-apploader: dynamic TA launched: {image_path} ({} bytes)",
        image.len()
    );
    Ok(())
}

fn probe_ping(cid: &str) -> io::Result<()> {
    let mut probe = TipcChannel::connect(&format!("VSOCK:{cid}:0"), PROBE_SERVICE)
        .map_err(|error| io::Error::new(io::ErrorKind::ConnectionRefused, error))?;
    probe.send(b"ping")?;
    let mut pong = [0; 4];
    recv_exact(&mut probe, &mut pong)?;
    if &pong != b"pong" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dynamic TA probe failed",
        ));
    }
    println!("ca-apploader: first dynamic TA responded to ping/pong");
    Ok(())
}

fn probe_bulk(cid: &str) -> io::Result<()> {
    let mut channel = TipcChannel::connect(&format!("VSOCK:{cid}:0"), BULK_PROBE_SERVICE)
        .map_err(|error| io::Error::new(io::ErrorKind::ConnectionRefused, error))?;
    let mut sent = vec![0; BULK_CHUNK_BYTES];
    let mut received = vec![0; BULK_CHUNK_BYTES];
    for offset in (0..BULK_TOTAL_BYTES).step_by(BULK_CHUNK_BYTES) {
        let chunk_bytes = (BULK_TOTAL_BYTES - offset).min(BULK_CHUNK_BYTES);
        let chunk_index = (offset / BULK_CHUNK_BYTES) as u8;
        for (index, byte) in sent[..chunk_bytes].iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(31).wrapping_add(chunk_index);
        }
        channel.send(&sent[..chunk_bytes])?;
        recv_exact(&mut channel, &mut received[..chunk_bytes])?;
        if sent[..chunk_bytes] != received[..chunk_bytes] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bulk TA echo mismatch at offset {offset}"),
            ));
        }
    }
    println!("ca-apploader: second dynamic TA echoed {BULK_TOTAL_BYTES} verified bytes");
    Ok(())
}

fn expect_status(channel: &mut TipcChannel, command: u32) -> io::Result<()> {
    let mut response = [0; 8];
    recv_exact(channel, &mut response)?;
    let response_command = u32::from_le_bytes(response[..4].try_into().unwrap());
    let status = u32::from_le_bytes(response[4..].try_into().unwrap());
    if response_command != (command | 1) || status != 0 {
        return Err(io::Error::other(format!(
            "apploader command {command} returned command {response_command}, status {status}"
        )));
    }
    Ok(())
}

fn recv_exact(channel: &mut TipcChannel, response: &mut [u8]) -> io::Result<()> {
    let mut received = 0;
    while received < response.len() {
        let count = channel.recv_no_alloc(&mut response[received..])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "apploader closed before responding",
            ));
        }
        received += count;
    }
    Ok(())
}
