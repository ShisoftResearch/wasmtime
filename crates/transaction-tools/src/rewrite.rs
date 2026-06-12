use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Default, Serialize, Eq, PartialEq)]
pub struct RewriteReport {
    pub transaction_functions: usize,
    pub persistent_addr_markers: usize,
    pub i32_tloads: usize,
    pub i64_tloads: usize,
    pub f32_tloads: usize,
    pub f64_tloads: usize,
    pub i32_tstores: usize,
    pub i64_tstores: usize,
    pub f32_tstores: usize,
    pub f64_tstores: usize,
}

pub fn rewrite_module(input: &[u8]) -> Result<(Vec<u8>, RewriteReport)> {
    Ok((input.to_vec(), RewriteReport::default()))
}

pub fn main() -> anyhow::Result<()> {
    println!("twasm-rust rewrite support is not initialized until the CLI task");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{RewriteReport, rewrite_module};

    #[test]
    fn rewrite_module_is_noop() {
        let input = b"\0asm\x01\0\0\0";
        let (output, report) = rewrite_module(input).expect("rewrite succeeds");

        assert_eq!(output, input);
        assert_eq!(report, RewriteReport::default());
    }
}
