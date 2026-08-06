// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal interactive shell for the EL2 runtime.

const LINE_CAPACITY: usize = 256;

pub(crate) fn run() {
    let mut line = [0_u8; LINE_CAPACITY];
    let mut line_len = 0;
    let mut previous_was_cr = false;

    kprintln!("XHyper EL2 runtime initialized");
    kprint!("xhyper> ");

    loop {
        let mut input = [0_u8; 1];
        if khal::console::read_data(&mut input) == 0 {
            ktask::yield_now();
            continue;
        }

        match input[0] {
            b'\r' | b'\n' => {
                if input[0] == b'\n' && previous_was_cr {
                    previous_was_cr = false;
                    continue;
                }
                previous_was_cr = input[0] == b'\r';
                kprintln!();
                execute(&line[..line_len]);
                line_len = 0;
                kprint!("xhyper> ");
            }
            8 | 127 => {
                previous_was_cr = false;
                if line_len != 0 {
                    line_len -= 1;
                    kprint!("\x08 \x08");
                }
            }
            byte if byte.is_ascii_graphic() || byte == b' ' => {
                previous_was_cr = false;
                if line_len < line.len() {
                    line[line_len] = byte;
                    line_len += 1;
                    kprint!("{}", char::from(byte));
                }
            }
            _ => previous_was_cr = false,
        }
    }
}

fn execute(line: &[u8]) {
    let Ok(line) = core::str::from_utf8(line) else {
        kprintln!("invalid command");
        return;
    };
    let line = line.trim();
    let (command, args) = line
        .split_once(char::is_whitespace)
        .map_or((line, ""), |(command, args)| (command, args.trim_start()));

    match command {
        "" => {}
        "echo" => kprintln!("{args}"),
        "exit" => khal::power::shutdown(),
        _ => kprintln!("unknown command: {command}"),
    }
}
