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

use common_expression::date_helper::DateConverter;
use common_expression::types::number::NumberScalar;
use common_expression::DataBlock;
use common_expression::ScalarRef;
use common_expression::TableSchemaRef;
use common_io::prelude::FormatSettings;
use serde_json::Value as JsonValue;

use crate::output_format::OutputFormat;
use crate::FileFormatOptionsExt;

pub struct JSONOutputFormat {
    schema: TableSchemaRef,
    first_block: bool,
    first_row: bool,
    rows: usize,
    format_settings: FormatSettings,
}

impl JSONOutputFormat {
    pub fn create(schema: TableSchemaRef, options: &FileFormatOptionsExt) -> Self {
        Self {
            schema,
            first_block: true,
            first_row: true,
            rows: 0,
            format_settings: FormatSettings {
                timezone: options.timezone,
            },
        }
    }

    fn format_schema(&self) -> common_exception::Result<Vec<u8>> {
        let fields = self.schema.fields();
        if fields.is_empty() {
            return Ok(b"\"meta\":[]".to_vec());
        }
        let mut res = b"\"meta\":[".to_vec();
        for field in fields {
            res.push(b'{');
            res.extend_from_slice(b"\"name\":\"");
            res.extend_from_slice(field.name().as_bytes());
            res.extend_from_slice(b"\",\"type\":\"");
            res.extend_from_slice(field.data_type().wrapped_display().as_bytes());
            res.extend_from_slice(b"\"}");
            res.push(b',');
        }
        res.pop();
        res.extend_from_slice(b"]");
        Ok(res)
    }
}

fn scalar_to_json(s: ScalarRef<'_>, format: &FormatSettings) -> JsonValue {
    match s {
        ScalarRef::Null => JsonValue::Null,
        ScalarRef::Boolean(v) => JsonValue::Bool(v),
        ScalarRef::Number(v) => match v {
            NumberScalar::Int8(v) => JsonValue::Number(v.into()),
            NumberScalar::Int16(v) => JsonValue::Number(v.into()),
            NumberScalar::Int32(v) => JsonValue::Number(v.into()),
            NumberScalar::Int64(v) => JsonValue::Number(v.into()),
            NumberScalar::UInt8(v) => JsonValue::Number(v.into()),
            NumberScalar::UInt16(v) => JsonValue::Number(v.into()),
            NumberScalar::UInt32(v) => JsonValue::Number(v.into()),
            NumberScalar::UInt64(v) => JsonValue::Number(v.into()),
            NumberScalar::Float32(v) => {
                JsonValue::Number(serde_json::Number::from_f64(f32::from(v) as f64).unwrap())
            }
            NumberScalar::Float64(v) => {
                JsonValue::Number(serde_json::Number::from_f64(v.into()).unwrap())
            }
        },
        ScalarRef::Decimal(_) => todo!("decimal"),
        ScalarRef::Date(v) => {
            let dt = DateConverter::to_date(&v, format.timezone);
            serde_json::to_value(dt.format("%Y-%m-%d").to_string()).unwrap()
        }
        ScalarRef::Timestamp(v) => {
            let dt = DateConverter::to_timestamp(&v, format.timezone);
            serde_json::to_value(dt.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap()
        }
        ScalarRef::EmptyArray => JsonValue::Array(vec![]),
        ScalarRef::String(x) => JsonValue::String(String::from_utf8_lossy(x).to_string()),
        ScalarRef::Array(x) => {
            let vals = x
                .iter()
                .map(|x| scalar_to_json(x.clone(), format))
                .collect();
            JsonValue::Array(vals)
        }
        ScalarRef::Tuple(x) => {
            let vals = x
                .iter()
                .enumerate()
                .map(|(idx, x)| (format!("{idx}"), scalar_to_json(x.clone(), format)))
                .collect();
            JsonValue::Object(vals)
        }
        ScalarRef::Variant(x) => {
            let b = common_jsonb::from_slice(x).unwrap();
            b.into()
        }
    }
}

impl OutputFormat for JSONOutputFormat {
    fn serialize_block(&mut self, data_block: &DataBlock) -> common_exception::Result<Vec<u8>> {
        let mut res = if self.first_block {
            self.first_block = false;
            let mut buf = b"{".to_vec();
            buf.extend_from_slice(self.format_schema()?.as_ref());
            buf.extend_from_slice(b",\"data\":[");
            buf
        } else {
            vec![]
        };

        let names = self
            .schema
            .fields()
            .iter()
            .map(|f| f.name().to_string())
            .collect::<Vec<String>>();

        self.rows += data_block.num_rows();
        let n_col = data_block.num_columns();
        for row in 0..data_block.num_rows() {
            if self.first_row {
                self.first_row = false;
            } else {
                res.push(b',');
            }
            res.push(b'{');
            for (c, value) in data_block.columns().iter().enumerate() {
                let value = value.value.as_ref();
                let scalar = unsafe { value.index_unchecked(row) };
                let value = scalar_to_json(scalar, &self.format_settings);

                res.push(b'\"');
                res.extend_from_slice(names[c].as_bytes());
                res.push(b'\"');

                res.push(b':');

                res.extend_from_slice(value.to_string().as_bytes());
                if c != n_col - 1 {
                    res.push(b',');
                }
            }
            res.push(b'}');
        }

        Ok(res)
    }

    fn finalize(&mut self) -> common_exception::Result<Vec<u8>> {
        let mut buf = b"".to_vec();
        if self.first_row {
            buf.push(b'{');
            buf.extend_from_slice(self.format_schema()?.as_ref());
            buf.extend_from_slice(b",\"data\":[");
        }
        buf.extend_from_slice(format!("],\"rows\":{}", self.rows).as_bytes());
        buf.push(b'}');
        buf.push(b'\n');
        Ok(buf)
    }
}
