// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub enum TARequest {
    Register { uuid: String },
}

#[derive(Serialize, Deserialize)]
pub enum TeeRequest {
    OpenSession {
        uuid: String,
        connection_method: u32,
        params: Parameters,
    },
    CloseSession {
        session_id: u32,
    },
    InvokeCommand {
        session_id: u32,
        cmd_id: u32,
        params: Parameters,
    },
    RequestCancellation {
        session_id: u32,
    },
}

#[derive(Serialize, Deserialize)]
pub enum TeeResponse {
    OpenSession { session_id: u32, result: u32 },
    CloseSession { result: u32 },
    InvokeCommand { params: Parameters, result: u32 },
    RequestCancellation { result: u32 },
}

#[derive(Serialize, Deserialize, Default)]
pub struct Parameters(pub Parameter, pub Parameter, pub Parameter, pub Parameter);

#[derive(Serialize, Deserialize, Default)]
pub struct Parameter {
    pub param: TeeParam,
    pub param_type: ParamType,
}

#[derive(Serialize, Deserialize, Default)]
pub struct TeeParam {
    pub data: Vec<u8>,
    pub values: Value,
}

#[derive(Serialize, Deserialize, Clone, Copy, Default)]
pub struct Value {
    pub a: u32,
    pub b: u32,
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq, Clone, Copy)]
pub enum ParamType {
    #[default]
    None = 0,
    ValueInput = 1,
    ValueOutput = 2,
    ValueInout = 3,
    MemrefInput = 5,
    MemrefOutput = 6,
    MemrefInout = 7,
}

impl From<u32> for ParamType {
    fn from(value: u32) -> Self {
        match value {
            0 => ParamType::None,
            1 => ParamType::ValueInput,
            2 => ParamType::ValueOutput,
            3 => ParamType::ValueInout,
            5 => ParamType::MemrefInput,
            6 => ParamType::MemrefOutput,
            7 => ParamType::MemrefInout,
            _ => ParamType::None,
        }
    }
}

impl From<ParamType> for u32 {
    fn from(value: ParamType) -> Self {
        match value {
            ParamType::None => 0,
            ParamType::ValueInput => 1,
            ParamType::ValueOutput => 2,
            ParamType::ValueInout => 3,
            ParamType::MemrefInput => 5,
            ParamType::MemrefOutput => 6,
            ParamType::MemrefInout => 7,
        }
    }
}
