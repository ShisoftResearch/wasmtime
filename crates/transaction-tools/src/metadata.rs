use anyhow::{Context, Result, bail};
use serde::Serialize;
use wasmparser::{Parser, Payload};

const PERSIST_SECTION_NAME: &str = "twasm.persist";
const PERSIST_RECORD_MAGIC: &[u8; 4] = b"TPRS";
const PERSIST_RECORD_VERSION: u8 = 1;
const PERSIST_HEADER_LEN: usize = 7;

#[derive(Debug, Default, Serialize, Eq, PartialEq)]
pub struct MetadataReport {
    pub persist_sections: usize,
    pub persist_payloads: Vec<String>,
}

pub fn inspect_module(bytes: &[u8]) -> Result<MetadataReport> {
    let mut report = MetadataReport::default();

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.context("failed to parse wasm payload")?;
        if let Payload::CustomSection(section) = payload {
            if section.name() == PERSIST_SECTION_NAME {
                report.persist_sections += 1;
                parse_persist_section(section.data(), &mut report.persist_payloads)?;
            }
        }
    }

    Ok(report)
}

fn parse_persist_section(bytes: &[u8], payloads: &mut Vec<String>) -> Result<()> {
    let mut offset = 0;

    while offset < bytes.len() {
        if bytes.len() - offset < PERSIST_HEADER_LEN {
            bail!(
                "truncated twasm.persist header at byte {}: expected at least {} bytes, found {}",
                offset,
                PERSIST_HEADER_LEN,
                bytes.len() - offset
            );
        }

        let record = &bytes[offset..];
        if &record[..4] != PERSIST_RECORD_MAGIC {
            bail!(
                "invalid twasm.persist magic at byte {}: expected {:?}, found {:?}",
                offset,
                PERSIST_RECORD_MAGIC,
                &record[..4]
            );
        }

        let version = record[4];
        if version != PERSIST_RECORD_VERSION {
            bail!(
                "unsupported twasm.persist version at byte {}: expected {}, found {}",
                offset + 4,
                PERSIST_RECORD_VERSION,
                version
            );
        }

        let name_len = usize::from(u16::from_le_bytes([record[5], record[6]]));
        let name_start = offset + PERSIST_HEADER_LEN;
        let name_end = name_start + name_len;
        if name_end > bytes.len() {
            bail!(
                "truncated twasm.persist name at byte {}: expected {} bytes, found {}",
                name_start,
                name_len,
                bytes.len().saturating_sub(name_start)
            );
        }

        let name = core::str::from_utf8(&bytes[name_start..name_end]).with_context(|| {
            format!(
                "invalid utf-8 in twasm.persist name at bytes {}..{}",
                name_start, name_end
            )
        })?;
        payloads.push(name.to_owned());
        offset = name_end;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MetadataReport, inspect_module};
    use wasm_encoder::{CustomSection, Module};

    fn persist_record_bytes(magic: [u8; 4], version: u8, name: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(7 + name.len());
        bytes.extend_from_slice(&magic);
        bytes.push(version);
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(name);
        bytes
    }

    fn module_with_persist_section(data: Vec<u8>) -> Vec<u8> {
        let mut module = Module::new();
        module.section(&CustomSection {
            name: "twasm.persist".into(),
            data: data.into(),
        });
        module.finish()
    }

    #[test]
    fn inspect_reports_bad_magic() {
        let bytes = module_with_persist_section(persist_record_bytes(*b"NOPE", 1, b"Bank"));
        let err = inspect_module(&bytes).unwrap_err().to_string();

        assert!(err.contains("invalid twasm.persist magic"));
    }

    #[test]
    fn inspect_reports_unsupported_version() {
        let bytes = module_with_persist_section(persist_record_bytes(*b"TPRS", 2, b"Bank"));
        let err = inspect_module(&bytes).unwrap_err().to_string();

        assert!(err.contains("unsupported twasm.persist version"));
    }

    #[test]
    fn inspect_reports_truncated_name() {
        let mut record = persist_record_bytes(*b"TPRS", 1, b"Bank");
        record.pop();
        let bytes = module_with_persist_section(record);
        let err = inspect_module(&bytes).unwrap_err().to_string();

        assert!(err.contains("truncated twasm.persist name"));
    }

    #[test]
    fn inspect_reports_truncated_header() {
        let bytes = module_with_persist_section(b"TP".to_vec());
        let err = inspect_module(&bytes).unwrap_err().to_string();

        assert!(err.contains("truncated twasm.persist header"));
    }

    #[test]
    fn inspect_reports_invalid_utf8() {
        let bytes = module_with_persist_section(persist_record_bytes(*b"TPRS", 1, &[0xff]));
        let err = inspect_module(&bytes).unwrap_err().to_string();

        assert!(err.contains("invalid utf-8 in twasm.persist name"));
    }

    #[test]
    fn inspect_ignores_other_custom_sections() {
        let mut module = Module::new();
        module.section(&CustomSection {
            name: "other".into(),
            data: b"ignored".as_slice().into(),
        });

        let report = inspect_module(&module.finish()).unwrap();
        assert_eq!(report, MetadataReport::default());
    }
}
