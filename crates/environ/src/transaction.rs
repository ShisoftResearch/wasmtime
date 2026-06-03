use crate::{WasmError, WasmResult};

/// Transaction opcode prefix used by Wizard and the simple-transactions
/// proposal branch.
pub const TRANSACTION_OPCODE_PREFIX: u8 = 0xfa;

/// Milestone-1 transaction operator decoded from the `0xfa` opcode space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionOperator {
    /// `ttry`
    TTry,
    /// `tfail`
    TFail,
    /// `tglobal.get`
    TGlobalGet,
    /// `tglobal.set`
    TGlobalSet,
    /// `i32.tload`
    I32TLoad,
    /// `i64.tload`
    I64TLoad,
    /// `f32.tload`
    F32TLoad,
    /// `f64.tload`
    F64TLoad,
    /// `i32.tload8_s`
    I32TLoad8S,
    /// `i32.tload8_u`
    I32TLoad8U,
    /// `i32.tload16_s`
    I32TLoad16S,
    /// `i32.tload16_u`
    I32TLoad16U,
    /// `i64.tload8_s`
    I64TLoad8S,
    /// `i64.tload8_u`
    I64TLoad8U,
    /// `i64.tload16_s`
    I64TLoad16S,
    /// `i64.tload16_u`
    I64TLoad16U,
    /// `i64.tload32_s`
    I64TLoad32S,
    /// `i64.tload32_u`
    I64TLoad32U,
    /// `i32.tstore`
    I32TStore,
    /// `i64.tstore`
    I64TStore,
    /// `f32.tstore`
    F32TStore,
    /// `f64.tstore`
    F64TStore,
    /// `i32.tstore8`
    I32TStore8,
    /// `i32.tstore16`
    I32TStore16,
    /// `i64.tstore8`
    I64TStore8,
    /// `i64.tstore16`
    I64TStore16,
    /// `i64.tstore32`
    I64TStore32,
    /// `tmemory.size`
    TMemorySize,
    /// `tmemory.grow`
    TMemoryGrow,
}

/// A decoded prefixed transaction operator and the number of bytes consumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedTransactionOperator {
    /// Decoded transaction operator.
    pub operator: TransactionOperator,
    /// Number of bytes consumed, including the `0xfa` prefix.
    pub bytes_read: usize,
}

/// Transaction operator found by the local research fixture parser bridge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResearchTransactionOperator {
    /// Byte offset of the `0xfa` prefix inside the scanned byte slice.
    pub offset: usize,
    /// Decoded transaction operator.
    pub operator: TransactionOperator,
    /// Number of bytes consumed, including the `0xfa` prefix.
    pub bytes_read: usize,
}

impl TransactionOperator {
    /// Returns this operator's Wizard/proposal `0xfa` subopcode.
    pub const fn subopcode(self) -> u32 {
        match self {
            Self::TTry => 0x04,
            Self::TFail => 0x0f,
            Self::TGlobalGet => 0x23,
            Self::TGlobalSet => 0x24,
            Self::I32TLoad => 0x28,
            Self::I64TLoad => 0x29,
            Self::F32TLoad => 0x2a,
            Self::F64TLoad => 0x2b,
            Self::I32TLoad8S => 0x2c,
            Self::I32TLoad8U => 0x2d,
            Self::I32TLoad16S => 0x2e,
            Self::I32TLoad16U => 0x2f,
            Self::I64TLoad8S => 0x30,
            Self::I64TLoad8U => 0x31,
            Self::I64TLoad16S => 0x32,
            Self::I64TLoad16U => 0x33,
            Self::I64TLoad32S => 0x34,
            Self::I64TLoad32U => 0x35,
            Self::I32TStore => 0x36,
            Self::I64TStore => 0x37,
            Self::F32TStore => 0x38,
            Self::F64TStore => 0x39,
            Self::I32TStore8 => 0x3a,
            Self::I32TStore16 => 0x3b,
            Self::I64TStore8 => 0x3c,
            Self::I64TStore16 => 0x3d,
            Self::I64TStore32 => 0x3e,
            Self::TMemorySize => 0x3f,
            Self::TMemoryGrow => 0x40,
        }
    }
}

/// Decode a milestone-1 transaction operator from raw prefixed opcode bytes.
///
/// The input must start with [`TRANSACTION_OPCODE_PREFIX`], followed by the
/// unsigned LEB128 subopcode used by Wizard's `0xfa` transaction opcode page.
pub fn decode_prefixed_milestone1_transaction_operator(
    bytes: &[u8],
) -> WasmResult<DecodedTransactionOperator> {
    if bytes.first().copied() != Some(TRANSACTION_OPCODE_PREFIX) {
        return Err(WasmError::InvalidWebAssembly {
            message: crate::__format!("expected transaction prefix {TRANSACTION_OPCODE_PREFIX:#x}"),
            offset: 0,
        });
    }
    let (subopcode, subopcode_len) = read_u32_leb(&bytes[1..], 1)?;
    Ok(DecodedTransactionOperator {
        operator: decode_milestone1_transaction_operator(subopcode)?,
        bytes_read: 1 + subopcode_len,
    })
}

/// Scan research fixture bytes for milestone-1 transaction operators.
///
/// This is intentionally a bridge, not a replacement for `wasmparser`. Callers
/// must pass a core function-body byte slice. The scanner ignores ordinary
/// operators and decodes only `0xfa` transaction-prefixed operators, preserving
/// their offsets for later generated-fixture diagnostics.
pub fn parse_research_transaction_operators(
    bytes: &[u8],
) -> WasmResult<alloc::vec::Vec<ResearchTransactionOperator>> {
    let mut operators = alloc::vec::Vec::new();
    let mut offset = 0;

    while offset < bytes.len() {
        if bytes[offset] != TRANSACTION_OPCODE_PREFIX {
            offset += 1;
            continue;
        }

        let decoded =
            decode_prefixed_milestone1_transaction_operator(&bytes[offset..]).map_err(|error| {
                match error {
                    WasmError::Unsupported(message) => WasmError::Unsupported(crate::__format!(
                        "{message} at fixture offset {offset}"
                    )),
                    WasmError::InvalidWebAssembly {
                        message,
                        offset: inner,
                    } => WasmError::InvalidWebAssembly {
                        message,
                        offset: offset + inner,
                    },
                    other => other,
                }
            })?;
        operators.push(ResearchTransactionOperator {
            offset,
            operator: decoded.operator,
            bytes_read: decoded.bytes_read,
        });
        offset += decoded.bytes_read;
    }

    Ok(operators)
}

/// Decode a milestone-1 transaction operator from a `0xfa` subopcode.
///
/// This is a research-branch bridge until the external `wasmparser` dependency
/// is patched or forked to expose first-class transaction operator variants.
pub fn decode_milestone1_transaction_operator(subopcode: u32) -> WasmResult<TransactionOperator> {
    let operator = match subopcode {
        0x04 => TransactionOperator::TTry,
        0x0f => TransactionOperator::TFail,
        0x23 => TransactionOperator::TGlobalGet,
        0x24 => TransactionOperator::TGlobalSet,
        0x28 => TransactionOperator::I32TLoad,
        0x29 => TransactionOperator::I64TLoad,
        0x2a => TransactionOperator::F32TLoad,
        0x2b => TransactionOperator::F64TLoad,
        0x2c => TransactionOperator::I32TLoad8S,
        0x2d => TransactionOperator::I32TLoad8U,
        0x2e => TransactionOperator::I32TLoad16S,
        0x2f => TransactionOperator::I32TLoad16U,
        0x30 => TransactionOperator::I64TLoad8S,
        0x31 => TransactionOperator::I64TLoad8U,
        0x32 => TransactionOperator::I64TLoad16S,
        0x33 => TransactionOperator::I64TLoad16U,
        0x34 => TransactionOperator::I64TLoad32S,
        0x35 => TransactionOperator::I64TLoad32U,
        0x36 => TransactionOperator::I32TStore,
        0x37 => TransactionOperator::I64TStore,
        0x38 => TransactionOperator::F32TStore,
        0x39 => TransactionOperator::F64TStore,
        0x3a => TransactionOperator::I32TStore8,
        0x3b => TransactionOperator::I32TStore16,
        0x3c => TransactionOperator::I64TStore8,
        0x3d => TransactionOperator::I64TStore16,
        0x3e => TransactionOperator::I64TStore32,
        0x3f => TransactionOperator::TMemorySize,
        0x40 => TransactionOperator::TMemoryGrow,
        _ => {
            return Err(WasmError::Unsupported(crate::__format!(
                "transaction operator 0xfa {subopcode:#x} is outside milestone 1"
            )));
        }
    };
    Ok(operator)
}

fn read_u32_leb(bytes: &[u8], offset: usize) -> WasmResult<(u32, usize)> {
    let mut result = 0u32;
    let mut shift = 0;

    for (i, byte) in bytes.iter().copied().enumerate().take(5) {
        let low_bits = u32::from(byte & 0x7f);
        result |= low_bits
            .checked_shl(shift)
            .ok_or_else(|| WasmError::InvalidWebAssembly {
                message: "transaction subopcode LEB shift overflow".into(),
                offset: offset + i,
            })?;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
    }

    Err(WasmError::InvalidWebAssembly {
        message: "transaction subopcode LEB is invalid or truncated".into(),
        offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_milestone_1_transaction_control_opcodes() {
        assert_eq!(
            decode_milestone1_transaction_operator(0x04).unwrap(),
            TransactionOperator::TTry
        );
        assert_eq!(
            decode_milestone1_transaction_operator(0x0f).unwrap(),
            TransactionOperator::TFail
        );
    }

    #[test]
    fn decodes_milestone_1_transaction_memory_opcodes() {
        assert_eq!(
            decode_milestone1_transaction_operator(0x28).unwrap(),
            TransactionOperator::I32TLoad
        );
        assert_eq!(
            decode_milestone1_transaction_operator(0x3e).unwrap(),
            TransactionOperator::I64TStore32
        );
        assert_eq!(
            decode_milestone1_transaction_operator(0x3f).unwrap(),
            TransactionOperator::TMemorySize
        );
        assert_eq!(
            decode_milestone1_transaction_operator(0x40).unwrap(),
            TransactionOperator::TMemoryGrow
        );
    }

    #[test]
    fn rejects_out_of_scope_transaction_opcodes() {
        let error = decode_milestone1_transaction_operator(0x25).unwrap_err();
        match error {
            WasmError::Unsupported(message) => {
                assert!(message.contains("transaction operator"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn decodes_prefixed_transaction_opcode_bytes() {
        let decoded =
            decode_prefixed_milestone1_transaction_operator(&[TRANSACTION_OPCODE_PREFIX, 0x28])
                .unwrap();
        assert_eq!(decoded.operator, TransactionOperator::I32TLoad);
        assert_eq!(decoded.bytes_read, 2);

        let error = decode_prefixed_milestone1_transaction_operator(&[0xfb, 0x28]).unwrap_err();
        match error {
            WasmError::InvalidWebAssembly { message, offset } => {
                assert!(message.contains("transaction prefix"));
                assert_eq!(offset, 0);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn local_parser_bridge_extracts_transaction_operators_from_function_body() {
        let operators = parse_research_transaction_operators(&[
            0x41,
            0x00,
            TRANSACTION_OPCODE_PREFIX,
            0x28,
            0x0b,
        ])
        .unwrap();

        assert_eq!(
            operators,
            [ResearchTransactionOperator {
                offset: 2,
                operator: TransactionOperator::I32TLoad,
                bytes_read: 2,
            }]
        );
    }

    #[test]
    fn local_parser_bridge_reports_transaction_operator_offset() {
        let error =
            parse_research_transaction_operators(&[TRANSACTION_OPCODE_PREFIX, 0x25]).unwrap_err();

        match error {
            WasmError::Unsupported(message) => {
                assert!(message.contains("0xfa 0x25"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
