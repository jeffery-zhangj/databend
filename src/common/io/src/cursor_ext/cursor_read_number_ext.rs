// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::BufRead;
use std::io::Cursor;

use common_exception::ErrorCode;
use common_exception::Result;
use lexical_core::FromLexical;

pub trait ReadNumberExt {
    fn read_int_text<T: FromLexical>(&mut self) -> Result<T>;
    fn read_float_text<T: FromLexical>(&mut self) -> Result<T>;

    fn read_num_text_exact<T: FromLexical>(&mut self) -> Result<T>;
}

fn collect_number(buffer: &[u8]) -> usize {
    let mut has_point = false;
    let mut has_number = false;
    let mut index = 0;
    let len = buffer.len();

    for _ in 0..len {
        match buffer[index] {
            b'0'..=b'9' => {
                has_number = true;
            }

            b'-' | b'+' => {
                if has_number {
                    break;
                }
            }
            b'.' => {
                has_point = true;
                index += 1;
                break;
            }
            _ => {
                break;
            }
        }
        index += 1;
    }
    if has_point {
        while index < len && (b'0'..=b'9').contains(&buffer[index]) {
            index += 1;
        }
    }

    if has_number && index < len && (buffer[index] == b'e' || buffer[index] == b'E') {
        index += 1;
        if index < len && (buffer[index] == b'-' || buffer[index] == b'+') {
            index += 1
        }
        while index < len && (b'0'..=b'9').contains(&buffer[index]) {
            index += 1;
        }
    }
    index
}

#[inline]
fn read_num_text_exact<T: FromLexical>(buf: &[u8]) -> Result<T> {
    match buf.is_empty() {
        true => Ok(T::default()),
        false => match FromLexical::from_lexical(buf) {
            Ok(value) => Ok(value),
            Err(cause) => Err(ErrorCode::BadBytes(format!(
                "Cannot parse value:{:?} to number type, cause: {:?}",
                String::from_utf8_lossy(buf),
                cause
            ))),
        },
    }
}

impl<B> ReadNumberExt for Cursor<B>
where B: AsRef<[u8]>
{
    fn read_int_text<T: FromLexical>(&mut self) -> Result<T> {
        let buf = self.remaining_slice();
        let idx = collect_number(buf);
        let n = read_num_text_exact(&buf[..idx])?;
        self.consume(idx);
        Ok(n)
    }

    fn read_float_text<T: FromLexical>(&mut self) -> Result<T> {
        let idx = collect_number(self.remaining_slice());
        let n = read_num_text_exact(&self.remaining_slice()[..idx])?;
        self.consume(idx);
        Ok(n)
    }

    fn read_num_text_exact<T: FromLexical>(&mut self) -> Result<T> {
        let buf = self.remaining_slice();
        read_num_text_exact(buf)
    }
}
