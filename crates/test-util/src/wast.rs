use serde::de::DeserializeOwned;
use serde_derive::Deserialize;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use wasmtime_environ::prelude::*;

/// Limits for running wast tests.
///
/// This is useful for sharing between `tests/wast.rs` and fuzzing, for
/// example, and is used as the minimum threshold for configuration when
/// fuzzing.
///
/// Note that it's ok to increase these numbers if a test comes along and needs
/// it, they're just here as empirically found minimum thresholds so far and
/// they're not too scientific.
pub mod limits {
    pub const MEMORY_SIZE: usize = 805 << 16;
    pub const MEMORIES: u32 = 450;
    pub const GC_HEAP_SIZE: usize = 10 << 16;
    pub const TABLES: u32 = 200;
    pub const MEMORIES_PER_MODULE: u32 = 9;
    pub const TABLES_PER_MODULE: u32 = 5;
    pub const COMPONENT_INSTANCES: u32 = 50;
    pub const CORE_INSTANCES: u32 = 900;
    pub const TABLE_ELEMENTS: usize = 1000;
    pub const CORE_INSTANCE_SIZE: usize = 64 * 1024;
    pub const TOTAL_STACKS: u32 = 20;
}

/// Local all `*.wast` tests under `root` which should be the path to the root
/// of the wasmtime repository.
pub fn find_tests(root: &Path) -> Result<Vec<WastTest>> {
    find_tests_with_config(root, TestDiscoveryConfig::default())
}

/// Configuration for discovering WAST tests.
#[derive(Clone, Copy, Debug)]
pub struct TestDiscoveryConfig {
    pub transaction_proposal: bool,
}

impl Default for TestDiscoveryConfig {
    fn default() -> Self {
        Self {
            transaction_proposal: std::env::var_os("WASMTIME_TEST_TRANSACTION_WAST").is_some(),
        }
    }
}

/// Local all `*.wast` tests under `root` with explicit discovery config.
pub fn find_tests_with_config(
    root: &Path,
    discovery: TestDiscoveryConfig,
) -> Result<Vec<WastTest>> {
    let mut tests = Vec::new();

    let spec_tests = root.join("tests/spec_testsuite");
    add_tests(
        &mut tests,
        &spec_tests,
        &FindConfig::Infer(spec_test_config),
    )
    .context("Do you need to `git submodule update --init`?")
    .with_context(|| format!("failed to add tests from `{}`", spec_tests.display()))?;

    let misc_tests = root.join("tests/misc_testsuite");
    add_tests(&mut tests, &misc_tests, &FindConfig::InTest)
        .with_context(|| format!("failed to add tests from `{}`", misc_tests.display()))?;

    let cm_tests = root.join("tests/component-model/test");
    add_tests(
        &mut tests,
        &cm_tests,
        &FindConfig::Infer(component_test_config),
    )
    .context("Do you need to `git submodule update --init`?")
    .with_context(|| format!("failed to add tests from `{}`", cm_tests.display()))?;

    if discovery.transaction_proposal {
        let proposal_root = root.join("../wasm-persistence/test/core");
        for (suite, dir) in [
            (
                TransactionProposalSuite::SimpleTransactions,
                "simple-transactions",
            ),
            (TransactionProposalSuite::Tsimd, "tsimd"),
        ] {
            let path = proposal_root.join(dir);
            add_tests(&mut tests, &path, &FindConfig::TransactionProposal(suite)).with_context(
                || {
                    format!(
                        "failed to add transactional Wasm proposal tests from `{}`",
                        path.display()
                    )
                },
            )?;
        }
    }

    // Temporarily work around upstream tests that fail in unexpected ways (e.g.
    // panics, loops, etc).
    {
        let skip_list = &[
            // .. empty currently ..
        ];
        tests.retain(|test| {
            test.path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| !skip_list.contains(&name))
                .unwrap_or(true)
        });
    }

    Ok(tests)
}

enum FindConfig {
    InTest,
    Infer(fn(&Path) -> TestConfig),
    TransactionProposal(TransactionProposalSuite),
}

fn add_tests(tests: &mut Vec<WastTest>, path: &Path, config: &FindConfig) -> Result<()> {
    for entry in path.read_dir().context("failed to read directory")? {
        let entry = entry.context("failed to read directory entry")?;
        let path = entry.path();
        if entry
            .file_type()
            .context("failed to get file type")?
            .is_dir()
        {
            add_tests(tests, &path, config).context("failed to read sub-directory")?;
            continue;
        }

        if path.extension().and_then(|s| s.to_str()) != Some("wast") {
            continue;
        }

        // These tests use `*.wast` directives not yet supported by Wasmtime, so
        // wait for a `wasm-tools` update to ungate these.
        if path.ends_with("spec_testsuite/custom/custom_annot.wast")
            || path.ends_with("spec_testsuite/custom/branch_hint.wast")
            || path.ends_with("spec_testsuite/custom/name_annot.wast")
        {
            continue;
        }

        let mut contents =
            fs::read_to_string(&path).with_context(|| format!("failed to read test: {path:?}"))?;
        let test_config = match config {
            FindConfig::InTest => parse_test_config(&contents, ";;!")
                .with_context(|| format!("failed to parse test configuration: {path:?}"))?,
            FindConfig::Infer(f) => f(&path),
            FindConfig::TransactionProposal(_) => transaction_proposal_test_config(&path),
        };
        let transaction_proposal = match config {
            FindConfig::TransactionProposal(suite) => Some(*suite),
            _ => None,
        };
        let transaction_real_text_parser = transaction_proposal
            .is_some_and(|suite| transaction_proposal_uses_real_text_parser(suite, &path));
        if transaction_proposal.is_some() && !transaction_real_text_parser {
            contents = normalize_transaction_proposal_wast(&contents);
        }
        tests.push(WastTest {
            path,
            contents,
            config: test_config,
            transaction_proposal,
            transaction_real_text_parser,
        })
    }
    Ok(())
}

fn spec_test_config(test: &Path) -> TestConfig {
    let mut ret = TestConfig::default();
    ret.spec_test = Some(true);
    ret.bulk_memory = Some(true);
    match spec_proposal_from_path(test) {
        Some("wide-arithmetic") => {
            ret.wide_arithmetic = Some(true);
        }
        Some("threads") => {
            ret.threads = Some(true);
            ret.reference_types = Some(false);
        }
        Some("custom-page-sizes") => {
            ret.custom_page_sizes = Some(true);
            ret.multi_memory = Some(true);
            ret.memory64 = Some(true);
            ret.reference_types = Some(true);

            // See commentary below in `wasm-3.0` case for why these "hog
            // memory"
            if test.ends_with("memory_max.wast") || test.ends_with("memory_max_i64.wast") {
                ret.hogs_memory = Some(true);
            }
        }
        Some("custom-descriptors") => {
            ret.custom_descriptors = Some(true);
        }
        Some(proposal) => panic!("unsupported proposal {proposal:?}"),
        None => {
            ret.reference_types = Some(true);
            ret.simd = Some(true);
            ret.simd = Some(true);
            ret.relaxed_simd = Some(true);
            ret.multi_memory = Some(true);
            ret.gc = Some(true);
            ret.reference_types = Some(true);
            ret.memory64 = Some(true);
            ret.tail_call = Some(true);
            ret.extended_const = Some(true);
            ret.exceptions = Some(true);

            if test.parent().unwrap().ends_with("legacy") {
                ret.legacy_exceptions = Some(true);
            }

            // These tests technically don't actually hog any memory but they
            // do have a module definition with a table/memory that is the
            // maximum size. These modules fail to compile in the pooling
            // allocator which has limits on the minimum size of
            // memories/tables by default.
            //
            // Pretend that these hog memory to avoid running the tests in the
            // pooling allocator.
            if test.ends_with("memory.wast")
                || test.ends_with("table.wast")
                || test.ends_with("memory64.wast")
                || test.ends_with("table64.wast")
            {
                ret.hogs_memory = Some(true);
            }
        }
    }

    ret
}

fn component_test_config(test: &Path) -> TestConfig {
    let mut ret = TestConfig::default();
    ret.spec_test = Some(true);
    ret.reference_types = Some(true);
    ret.multi_memory = Some(true);

    if let Some(parent) = test.parent() {
        if parent.ends_with("async")
            || [
                "trap-in-post-return.wast",
                "resources.wast",
                "multiple-resources.wast",
            ]
            .into_iter()
            .any(|name| Some(name) == test.file_name().and_then(|s| s.to_str()))
        {
            ret.component_model_async = Some(true);
            ret.component_model_async_stackful = Some(true);
            ret.component_model_more_async_builtins = Some(true);
            ret.component_model_threading = Some(true);
        }
        if parent.ends_with("wasm-tools") {
            ret.memory64 = Some(true);
            ret.threads = Some(true);
            ret.exceptions = Some(true);
            ret.gc = Some(true);
        }
        if parent.ends_with("wasmtime") {
            ret.exceptions = Some(true);
            ret.gc = Some(true);
        }
    }

    ret
}

fn transaction_proposal_test_config(test: &Path) -> TestConfig {
    let mut ret = TestConfig::default();
    ret.bulk_memory = Some(true);
    ret.gc = Some(true);
    ret.reference_types = Some(true);
    ret.function_references = Some(true);
    ret.tail_call = Some(true);

    if test
        .parent()
        .is_some_and(|parent| parent.ends_with("tsimd"))
    {
        ret.simd = Some(true);
    }

    ret
}

fn normalize_transaction_proposal_wast(wast: &str) -> String {
    let mut out = String::with_capacity(wast.len());
    let mut chars = wast.char_indices().peekable();
    let mut lists: Vec<ListContext> = Vec::new();

    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => {
                let end = string_end(wast, idx);
                let quote_module = lists.last().is_some_and(|tokens| {
                    tokens.tokens.len() >= 2
                        && tokens.tokens[0] == "module"
                        && tokens.tokens[1] == "quote"
                });
                let import_field_name = lists
                    .last()
                    .is_some_and(ListContext::is_spectest_import_field_name);
                let diagnostic = lists.last().is_some_and(ListContext::is_assert_diagnostic);
                out.push_str(&normalize_transaction_string(
                    &wast[idx..end],
                    quote_module,
                    import_field_name,
                    diagnostic,
                ));
                if let Some(tokens) = lists.last_mut() {
                    if let Some(body) = wast[idx..end]
                        .strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                    {
                        tokens.tokens.push(body.to_string());
                    }
                    tokens.string_count += 1;
                }
                while chars.peek().is_some_and(|(next, _)| *next < end) {
                    chars.next();
                }
            }
            ';' if chars.peek().is_some_and(|(_, next)| *next == ';') => {
                let end = wast[idx..]
                    .find('\n')
                    .map_or(wast.len(), |newline| idx + newline);
                out.push_str(&wast[idx..end]);
                while chars.peek().is_some_and(|(next, _)| *next < end) {
                    chars.next();
                }
            }
            '(' if chars.peek().is_some_and(|(_, next)| *next == ';') => {
                let end = block_comment_end(wast, idx);
                out.push_str(&wast[idx..end]);
                while chars.peek().is_some_and(|(next, _)| *next < end) {
                    chars.next();
                }
            }
            _ if ch.is_whitespace() => out.push(ch),
            '(' => {
                lists.push(ListContext::default());
                out.push(ch);
            }
            ')' => {
                lists.pop();
                out.push(ch);
            }
            _ => {
                let end = token_end(wast, idx);
                let token = &wast[idx..end];
                out.push_str(normalize_transaction_token(token));
                if let Some(tokens) = lists.last_mut() {
                    tokens.tokens.push(token.to_string());
                }
                while chars.peek().is_some_and(|(next, _)| *next < end) {
                    chars.next();
                }
            }
        }
    }

    out
}

#[derive(Default)]
struct ListContext {
    tokens: Vec<String>,
    string_count: usize,
}

impl ListContext {
    fn is_spectest_import_field_name(&self) -> bool {
        self.tokens.first().is_some_and(|token| token == "import")
            && self.string_count == 1
            && self.tokens.get(1).is_some_and(|token| token == "spectest")
    }

    fn is_assert_diagnostic(&self) -> bool {
        self.tokens.first().is_some_and(|token| {
            matches!(
                token.as_str(),
                "assert_invalid" | "assert_malformed" | "assert_trap" | "assert_exhaustion"
            )
        })
    }
}

fn normalize_transaction_string(
    string: &str,
    quote_module: bool,
    import_field_name: bool,
    diagnostic: bool,
) -> String {
    let Some(body) = string.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return string.to_string();
    };

    if quote_module {
        let decoded = decode_module_quote_body(body);
        let normalized = normalize_transaction_proposal_wast(&decoded);
        return format!("\"{}\"", encode_module_quote_body(&normalized));
    }

    if import_field_name {
        let normalized = normalize_transaction_import_name(body);
        if normalized != body {
            return format!("\"{normalized}\"");
        }
    }

    if diagnostic {
        let normalized = normalize_transaction_diagnostic(body);
        if normalized != body {
            return format!("\"{normalized}\"");
        }
    }

    string.to_string()
}

fn decode_module_quote_body(body: &str) -> String {
    let mut decoded = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('"') => decoded.push('"'),
                Some(next) => {
                    decoded.push('\\');
                    decoded.push(next);
                }
                None => decoded.push('\\'),
            }
        } else {
            decoded.push(ch);
        }
    }
    decoded
}

fn encode_module_quote_body(body: &str) -> String {
    body.replace('"', "\\\"")
}

fn normalize_transaction_import_name(name: &str) -> &str {
    match name {
        "tprint" => "print",
        "tprint_i32" => "print_i32",
        "tprint_i64" => "print_i64",
        "tprint_f32" => "print_f32",
        "tprint_f64" => "print_f64",
        "tprint_i32_f32" => "print_i32_f32",
        "tprint_f64_f64" => "print_f64_f64",
        "tmemory" => "memory",
        "tglobal_i32" => "global_i32",
        "tglobal_i64" => "global_i64",
        "tglobal_f32" => "global_f32",
        "tglobal_f64" => "global_f64",
        "ttable" => "table",
        _ => name,
    }
}

fn normalize_transaction_token(token: &str) -> &str {
    match token {
        "return_tcall_indirect" => "return_call_indirect",
        "return_tcall_ref" => "return_call_ref",
        "return_tcall" => "return_call",
        "tcall_indirect" => "call_indirect",
        "tcall_ref" => "call_ref",
        "tcall" => "call",
        "tinvoke" => "invoke",
        "tget" => "get",
        "tfuncref" => "funcref",
        "tfunc" => "func",
        "ttable.size" => "table.size",
        "ttable.grow" => "table.grow",
        "ttable.fill" => "table.fill",
        "ttable.copy" => "table.copy",
        "ttable.init" => "table.init",
        "ttable.get" => "table.get",
        "ttable.set" => "table.set",
        "ttable" => "table",
        "telem.drop" => "elem.drop",
        "telem" => "elem",
        "tmemory.grow" => "memory.grow",
        "tmemory.size" => "memory.size",
        "tmemory.copy" => "memory.copy",
        "tmemory.fill" => "memory.fill",
        "tmemory.init" => "memory.init",
        "tglobal.get" => "global.get",
        "tglobal.set" => "global.set",
        "tglobal" => "global",
        "tmemory" => "memory",
        "tdata.drop" => "data.drop",
        "tdata" => "data",
        "v128.tload" => "v128.load",
        "v128.tload8_splat" => "v128.load8_splat",
        "v128.tload16_splat" => "v128.load16_splat",
        "v128.tload32_splat" => "v128.load32_splat",
        "v128.tload64_splat" => "v128.load64_splat",
        "v128.tload8x8_s" => "v128.load8x8_s",
        "v128.tload8x8_u" => "v128.load8x8_u",
        "v128.tload16x4_s" => "v128.load16x4_s",
        "v128.tload16x4_u" => "v128.load16x4_u",
        "v128.tload32x2_s" => "v128.load32x2_s",
        "v128.tload32x2_u" => "v128.load32x2_u",
        "v128.tload32_zero" => "v128.load32_zero",
        "v128.tload64_zero" => "v128.load64_zero",
        "v128.tload8_lane" => "v128.load8_lane",
        "v128.tload16_lane" => "v128.load16_lane",
        "v128.tload32_lane" => "v128.load32_lane",
        "v128.tload64_lane" => "v128.load64_lane",
        "v128.tstore" => "v128.store",
        "v128.tstore8_lane" => "v128.store8_lane",
        "v128.tstore16_lane" => "v128.store16_lane",
        "v128.tstore32_lane" => "v128.store32_lane",
        "v128.tstore64_lane" => "v128.store64_lane",
        _ => normalize_load_store_token(token),
    }
}

fn normalize_load_store_token(token: &str) -> &str {
    match token {
        "i32.tload" => "i32.load",
        "i32.tload8_s" => "i32.load8_s",
        "i32.tload8_u" => "i32.load8_u",
        "i32.tload16_s" => "i32.load16_s",
        "i32.tload16_u" => "i32.load16_u",
        "i64.tload" => "i64.load",
        "i64.tload8_s" => "i64.load8_s",
        "i64.tload8_u" => "i64.load8_u",
        "i64.tload16_s" => "i64.load16_s",
        "i64.tload16_u" => "i64.load16_u",
        "i64.tload32_s" => "i64.load32_s",
        "i64.tload32_u" => "i64.load32_u",
        "f32.tload" => "f32.load",
        "f64.tload" => "f64.load",
        "i32.tstore" => "i32.store",
        "i32.tstore8" => "i32.store8",
        "i32.tstore16" => "i32.store16",
        "i64.tstore" => "i64.store",
        "i64.tstore8" => "i64.store8",
        "i64.tstore16" => "i64.store16",
        "i64.tstore32" => "i64.store32",
        "f32.tstore" => "f32.store",
        "f64.tstore" => "f64.store",
        _ => token,
    }
}

fn normalize_transaction_diagnostic(text: &str) -> String {
    let replacements = [
        ("undefined telement", "undefined element"),
        ("uninitialized telement", "uninitialized element"),
        (
            "indirect tcall type mismatch",
            "indirect call type mismatch",
        ),
        (
            "out of bounds tmemory access",
            "out of bounds memory access",
        ),
        (
            "memory size must be at most 65536 pages (4GiB)",
            "memory size must be at most 0x10000 65536-byte pages",
        ),
        ("multiple tmemories", "multiple memories"),
        ("unknown tmemory", "unknown memory"),
        ("inline tfunction type", "inline function type"),
        ("null tfunction", "null function"),
        ("unknown tglobal", "unknown global"),
        ("invalid lane index", "SIMD index out of bounds"),
    ];

    replacements
        .into_iter()
        .fold(text.to_string(), |text, (from, to)| text.replace(from, to))
}

fn string_end(wast: &str, start: usize) -> usize {
    let mut escaped = false;
    for (idx, ch) in wast[start + 1..].char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return start + 1 + idx + ch.len_utf8();
        }
    }
    wast.len()
}

fn block_comment_end(wast: &str, start: usize) -> usize {
    let mut depth = 0usize;
    let mut iter = wast[start..].char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        if ch == '(' && iter.peek().is_some_and(|(_, next)| *next == ';') {
            depth += 1;
            iter.next();
        } else if ch == ';' && iter.peek().is_some_and(|(_, next)| *next == ')') {
            iter.next();
            depth -= 1;
            if depth == 0 {
                return start + idx + 2;
            }
        }
    }
    wast.len()
}

fn token_end(wast: &str, start: usize) -> usize {
    for (idx, ch) in wast[start..].char_indices().skip(1) {
        if ch.is_whitespace() || matches!(ch, '(' | ')' | '"') || ch == ';' {
            return start + idx;
        }
    }
    wast.len()
}

/// Parse test configuration from the specified test, comments starting with
/// `;;!`.
pub fn parse_test_config<T>(wat: &str, comment: &'static str) -> Result<T>
where
    T: DeserializeOwned,
{
    // The test config source is the leading lines of the WAT file that are
    // prefixed with `;;!`.
    let config_lines: Vec<_> = wat
        .lines()
        .take_while(|l| l.starts_with(comment))
        .map(|l| &l[comment.len()..])
        .collect();
    let config_text = config_lines.join("\n");

    toml::from_str(&config_text).context("failed to parse the test configuration")
}

/// A `*.wast` test with its path, contents, and configuration.
#[derive(Clone)]
pub struct WastTest {
    pub path: PathBuf,
    pub contents: String,
    pub config: TestConfig,
    pub transaction_proposal: Option<TransactionProposalSuite>,
    pub transaction_real_text_parser: bool,
}

impl fmt::Debug for WastTest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WastTest")
            .field("path", &self.path)
            .field("contents", &"...")
            .field("config", &self.config)
            .field("transaction_proposal", &self.transaction_proposal)
            .field(
                "transaction_real_text_parser",
                &self.transaction_real_text_parser,
            )
            .finish()
    }
}

/// Transactional Wasm proposal corpus that a WAST test came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionProposalSuite {
    SimpleTransactions,
    Tsimd,
}

impl TransactionProposalSuite {
    pub fn name(self) -> &'static str {
        match self {
            Self::SimpleTransactions => "simple-transactions",
            Self::Tsimd => "tsimd",
        }
    }
}

macro_rules! foreach_config_option {
    ($m:ident) => {
        $m! {
            bulk_memory
            memory64
            custom_page_sizes
            multi_memory
            threads
            shared_everything_threads
            gc
            function_references
            relaxed_simd
            reference_types
            tail_call
            extended_const
            wide_arithmetic
            branch_hinting
            hogs_memory
            nan_canonicalization
            component_model_async
            component_model_more_async_builtins
            component_model_async_stackful
            component_model_threading
            component_model_error_context
            component_model_gc
            component_model_map
            component_model_fixed_length_lists
            component_model_implements
            simd
            gc_types
            exceptions
            legacy_exceptions
            stack_switching
            spec_test
            custom_descriptors
        }
    };
}

macro_rules! define_test_config {
    ($($option:ident)*) => {
        /// Per-test configuration which is written down in the test file itself for
        /// `misc_testsuite/**/*.wast` or in `spec_test_config` above for spec tests.
        #[derive(Debug, PartialEq, Default, Deserialize, Clone)]
        #[serde(deny_unknown_fields)]
        pub struct TestConfig {
            $(pub $option: Option<bool>,)*
        }

        impl TestConfig {
            $(
                pub fn $option(&self) -> bool {
                    self.$option.unwrap_or(false)
                }
            )*
        }
    }
}

foreach_config_option!(define_test_config);

impl TestConfig {
    /// Returns an iterator over each option.
    pub fn options_mut(&mut self) -> impl Iterator<Item = (&'static str, &mut Option<bool>)> {
        macro_rules! mk {
            ($($option:ident)*) => {
                [
                    $((stringify!($option), &mut self.$option),)*
                ].into_iter()
            }
        }
        foreach_config_option!(mk)
    }
}

/// Configuration that spec tests can run under.
#[derive(Debug)]
pub struct WastConfig {
    /// Compiler chosen to run this test.
    pub compiler: Compiler,
    /// Whether or not the pooling allocator is enabled.
    pub pooling: bool,
    /// What garbage collector is being used.
    pub collector: Collector,
}

/// Different compilers that can be tested in Wasmtime.
#[derive(PartialEq, Debug, Copy, Clone)]
pub enum Compiler {
    /// Cranelift backend.
    ///
    /// This tests the Cranelift code generator for native platforms. This
    /// notably excludes Pulley since that's listed separately below even though
    /// Pulley is a backend of Cranelift. This is only used for native code
    /// generation such as x86_64.
    CraneliftNative,

    /// Winch backend.
    ///
    /// This tests the Winch backend for native platforms. Currently Winch
    /// primarily supports x86_64.
    Winch,

    /// Pulley interpreter.
    ///
    /// This tests the Cranelift pulley backend plus the pulley execution
    /// environment of the output bytecode. Note that this is separate from
    /// `Cranelift` above to be able to test both on platforms where Cranelift
    /// has native codegen support.
    CraneliftPulley,
}

impl Compiler {
    /// Returns whether this compiler is known to fail for the provided
    /// `TestConfig`.
    ///
    /// This function will determine if the configuration of the test provided
    /// is known to guarantee fail. This effectively tracks the proposal support
    /// for each compiler backend/runtime and tests whether `config` enables or
    /// disables features that aren't supported.
    ///
    /// Note that this is closely aligned with
    /// `Config::compiler_panicking_wasm_features`.
    pub fn should_fail(&self, config: &TestConfig) -> bool {
        match self {
            Compiler::CraneliftNative => {
                if config.legacy_exceptions() {
                    return true;
                }

                // Stack-switching is only implemented on x86_64 for unix
                // platforms right now.
                if config.stack_switching() && !(cfg!(target_arch = "x86_64") && cfg!(unix)) {
                    return true;
                }

                false
            }

            Compiler::Winch => {
                if config.gc()
                    || config.tail_call()
                    || config.function_references()
                    || config.gc()
                    || config.relaxed_simd()
                    || config.gc_types()
                    || config.exceptions()
                    || config.legacy_exceptions()
                    || config.stack_switching()
                    || config.legacy_exceptions()
                    || config.component_model_async()
                {
                    return true;
                }

                if cfg!(target_arch = "aarch64") {
                    return (config.simd() && !config.spec_test()) || config.threads();
                }

                !cfg!(target_arch = "x86_64")
            }

            Compiler::CraneliftPulley => {
                config.threads() || config.legacy_exceptions() || config.stack_switching()
            }
        }
    }

    /// Returns whether this compiler configuration supports the current host
    /// architecture.
    pub fn supports_host(&self) -> bool {
        match self {
            Compiler::CraneliftNative => {
                cfg!(target_arch = "x86_64")
                    || cfg!(target_arch = "aarch64")
                    || cfg!(target_arch = "riscv64")
                    || cfg!(target_arch = "s390x")
            }
            Compiler::Winch => cfg!(target_arch = "x86_64") || cfg!(target_arch = "aarch64"),
            Compiler::CraneliftPulley => true,
        }
    }
}

#[derive(PartialEq, Debug, Copy, Clone)]
pub enum Collector {
    Auto,
    Null,
    DeferredReferenceCounting,
    Copying,
}

impl WastTest {
    /// Returns the transactional Wasm proposal suite that this test came from.
    pub fn transaction_proposal(&self) -> Option<TransactionProposalSuite> {
        self.transaction_proposal
    }

    /// Returns whether this proposal test uses the real transaction text parser
    /// instead of the research normalization adapter.
    pub fn transaction_real_text_parser(&self) -> bool {
        self.transaction_real_text_parser
    }

    /// Returns whether this transactional proposal test can currently run.
    pub fn transaction_proposal_enabled(&self) -> bool {
        if self.transaction_real_text_parser {
            return false;
        }
        let Some(suite) = self.transaction_proposal else {
            return false;
        };
        let Some(name) = self.path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };

        match suite {
            TransactionProposalSuite::SimpleTransactions => {
                simple_transaction_proposal_enabled(name)
            }
            TransactionProposalSuite::Tsimd => tsimd_transaction_proposal_enabled(name),
        }
    }

    /// Returns whether this test exercises the GC types and might want to use
    /// multiple different garbage collectors.
    pub fn test_uses_gc_types(&self) -> bool {
        self.config.gc() || self.config.function_references()
    }

    /// Returns the optional spec proposal that this test is associated with.
    pub fn spec_proposal(&self) -> Option<&str> {
        spec_proposal_from_path(&self.path)
    }

    /// Returns whether this test should fail under the specified extra
    /// configuration.
    pub fn should_fail(&self, config: &WastConfig) -> bool {
        if !config.compiler.supports_host() {
            return true;
        }

        let unsupported = [
            // These tests in the `component-model` submodule have not yet been
            // updated to account for the recent threading-related intrinsic
            // changes.
            "test/async/trap-if-block-and-sync.wast",
            // Wasmtime doesn't expose the component-model `cm64` feature toggle
            // yet, so this parser-only test can't be enabled here.
            "test/wasm-tools/memory64.wast",
        ];
        if unsupported.iter().any(|part| self.path.ends_with(part)) {
            return true;
        }

        // Some tests are known to fail with the pooling allocator
        if config.pooling {
            // allocates too much memory for the pooling configuration here
            if self.config.hogs_memory() {
                return true;
            }
            let unsupported = [
                // shared memories + pooling allocator aren't supported yet
                "misc_testsuite/memory-combos.wast",
                "misc_testsuite/threads/atomics-end-of-memory.wast",
                "misc_testsuite/threads/LB.wast",
                "misc_testsuite/threads/LB_atomic.wast",
                "misc_testsuite/threads/MP.wast",
                "misc_testsuite/threads/MP_atomic.wast",
                "misc_testsuite/threads/MP_wait.wast",
                "misc_testsuite/threads/SB.wast",
                "misc_testsuite/threads/SB_atomic.wast",
                "misc_testsuite/threads/atomics_notify.wast",
                "misc_testsuite/threads/atomics_wait_address.wast",
                "misc_testsuite/threads/wait_notify.wast",
                "spec_testsuite/proposals/threads/atomic.wast",
                "spec_testsuite/proposals/threads/exports.wast",
                "spec_testsuite/proposals/threads/memory.wast",
                "misc_testsuite/memory64/threads.wast",
                "misc_testsuite/winch/rmw32_cmpxchg_u_wrap.wast",
            ];

            if unsupported.iter().any(|part| self.path.ends_with(part)) {
                return true;
            }
        }

        if config.compiler.should_fail(&self.config) {
            return true;
        }

        // Disable spec tests per target for proposals that Winch does not implement yet.
        if config.compiler == Compiler::Winch {
            // Common list for tests that fail in all targets supported by Winch.
            let unsupported = [
                "extended-const/elem.wast",
                "extended-const/global.wast",
                "misc_testsuite/component-model/modules.wast",
                "misc_testsuite/externref-id-function.wast",
                "misc_testsuite/externref-segment.wast",
                "misc_testsuite/externref-segments.wast",
                "misc_testsuite/externref-table-dropped-segment-issue-8281.wast",
                "misc_testsuite/linking-errors.wast",
                "misc_testsuite/many_table_gets_lead_to_gc.wast",
                "misc_testsuite/mutable_externref_globals.wast",
                "misc_testsuite/no-mixup-stack-maps.wast",
                "misc_testsuite/no-panic.wast",
                "misc_testsuite/simple_ref_is_null.wast",
            ];

            if unsupported.iter().any(|part| self.path.ends_with(part)) {
                return true;
            }

            #[cfg(target_arch = "aarch64")]
            {
                let unsupported = [
                    "misc_testsuite/int-to-float-splat.wast",
                    "misc_testsuite/issue6562.wast",
                    "misc_testsuite/memory64/simd.wast",
                    "misc_testsuite/simd/almost-extmul.wast",
                    "misc_testsuite/simd/canonicalize-nan.wast",
                    "misc_testsuite/simd/cvt-from-uint.wast",
                    "misc_testsuite/simd/edge-of-memory.wast",
                    "misc_testsuite/simd/interesting-float-splat.wast",
                    "misc_testsuite/simd/issue4807.wast",
                    "misc_testsuite/simd/issue6725-no-egraph-panic.wast",
                    "misc_testsuite/simd/issue_3173_select_v128.wast",
                    "misc_testsuite/simd/issue_3327_bnot_lowering.wast",
                    "misc_testsuite/simd/load_splat_out_of_bounds.wast",
                    "misc_testsuite/simd/replace-lane-preserve.wast",
                    "misc_testsuite/simd/spillslot-size-fuzzbug.wast",
                    "misc_testsuite/simd/sse-cannot-fold-unaligned-loads.wast",
                    "misc_testsuite/simd/unaligned-load.wast",
                    "misc_testsuite/simd/v128-select.wast",
                    "misc_testsuite/winch/issue-10331.wast",
                    "misc_testsuite/winch/issue-10357.wast",
                    "misc_testsuite/winch/issue-10460.wast",
                    "misc_testsuite/winch/replace_lane.wast",
                    "misc_testsuite/winch/simd_multivalue.wast",
                    "misc_testsuite/winch/v128_load_lane_invalid_address.wast",
                    "spec_testsuite/proposals/annotations/simd_lane.wast",
                    "spec_testsuite/proposals/multi-memory/simd_memory-multi.wast",
                    "spec_testsuite/simd_address.wast",
                    "spec_testsuite/simd_align.wast",
                    "spec_testsuite/simd_bit_shift.wast",
                    "spec_testsuite/simd_bitwise.wast",
                    "spec_testsuite/simd_boolean.wast",
                    "spec_testsuite/simd_const.wast",
                    "spec_testsuite/simd_conversions.wast",
                    "spec_testsuite/simd_f32x4.wast",
                    "spec_testsuite/simd_f32x4_arith.wast",
                    "spec_testsuite/simd_f32x4_cmp.wast",
                    "spec_testsuite/simd_f32x4_pmin_pmax.wast",
                    "spec_testsuite/simd_f32x4_rounding.wast",
                    "spec_testsuite/simd_f64x2.wast",
                    "spec_testsuite/simd_f64x2_arith.wast",
                    "spec_testsuite/simd_f64x2_cmp.wast",
                    "spec_testsuite/simd_f64x2_pmin_pmax.wast",
                    "spec_testsuite/simd_f64x2_rounding.wast",
                    "spec_testsuite/simd_i16x8_arith.wast",
                    "spec_testsuite/simd_i16x8_arith2.wast",
                    "spec_testsuite/simd_i16x8_cmp.wast",
                    "spec_testsuite/simd_i16x8_extadd_pairwise_i8x16.wast",
                    "spec_testsuite/simd_i16x8_extmul_i8x16.wast",
                    "spec_testsuite/simd_i16x8_q15mulr_sat_s.wast",
                    "spec_testsuite/simd_i16x8_sat_arith.wast",
                    "spec_testsuite/simd_i32x4_arith.wast",
                    "spec_testsuite/simd_i32x4_arith2.wast",
                    "spec_testsuite/simd_i32x4_cmp.wast",
                    "spec_testsuite/simd_i32x4_dot_i16x8.wast",
                    "spec_testsuite/simd_i32x4_extadd_pairwise_i16x8.wast",
                    "spec_testsuite/simd_i32x4_extmul_i16x8.wast",
                    "spec_testsuite/simd_i32x4_trunc_sat_f32x4.wast",
                    "spec_testsuite/simd_i32x4_trunc_sat_f64x2.wast",
                    "spec_testsuite/simd_i64x2_arith.wast",
                    "spec_testsuite/simd_i64x2_arith2.wast",
                    "spec_testsuite/simd_i64x2_cmp.wast",
                    "spec_testsuite/simd_i64x2_extmul_i32x4.wast",
                    "spec_testsuite/simd_i8x16_arith.wast",
                    "spec_testsuite/simd_i8x16_arith2.wast",
                    "spec_testsuite/simd_i8x16_cmp.wast",
                    "spec_testsuite/simd_i8x16_sat_arith.wast",
                    "spec_testsuite/simd_int_to_int_extend.wast",
                    "spec_testsuite/simd_lane.wast",
                    "spec_testsuite/simd_load.wast",
                    "spec_testsuite/simd_load16_lane.wast",
                    "spec_testsuite/simd_load32_lane.wast",
                    "spec_testsuite/simd_load64_lane.wast",
                    "spec_testsuite/simd_load8_lane.wast",
                    "spec_testsuite/simd_load_extend.wast",
                    "spec_testsuite/simd_load_splat.wast",
                    "spec_testsuite/simd_load_zero.wast",
                    "spec_testsuite/simd_select.wast",
                    "spec_testsuite/simd_splat.wast",
                    "spec_testsuite/simd_store.wast",
                    "spec_testsuite/simd_store16_lane.wast",
                    "spec_testsuite/simd_store32_lane.wast",
                    "spec_testsuite/simd_store64_lane.wast",
                    "spec_testsuite/simd_store8_lane.wast",
                ];

                if unsupported.iter().any(|part| self.path.ends_with(part)) {
                    return true;
                }
            }

            #[cfg(target_arch = "x86_64")]
            {
                // SIMD on Winch requires AVX instructions.
                #[cfg(target_arch = "x86_64")]
                if !(std::is_x86_feature_detected!("avx") && std::is_x86_feature_detected!("avx2"))
                {
                    let unsupported = [
                        "annotations/simd_lane.wast",
                        "memory64/simd.wast",
                        "misc_testsuite/int-to-float-splat.wast",
                        "misc_testsuite/issue6562.wast",
                        "misc_testsuite/simd/almost-extmul.wast",
                        "misc_testsuite/simd/canonicalize-nan.wast",
                        "misc_testsuite/simd/cvt-from-uint.wast",
                        "misc_testsuite/simd/edge-of-memory.wast",
                        "misc_testsuite/simd/issue_3327_bnot_lowering.wast",
                        "misc_testsuite/simd/issue6725-no-egraph-panic.wast",
                        "misc_testsuite/simd/replace-lane-preserve.wast",
                        "misc_testsuite/simd/spillslot-size-fuzzbug.wast",
                        "misc_testsuite/simd/sse-cannot-fold-unaligned-loads.wast",
                        "misc_testsuite/winch/issue-10331.wast",
                        "misc_testsuite/winch/replace_lane.wast",
                        "misc_testsuite/simd/riscv64-replicated-imm5-works.wast",
                        "misc_testsuite/simd/v128-equal.wast",
                        "spec_testsuite/simd_align.wast",
                        "spec_testsuite/simd_boolean.wast",
                        "spec_testsuite/simd_conversions.wast",
                        "spec_testsuite/simd_f32x4.wast",
                        "spec_testsuite/simd_f32x4_arith.wast",
                        "spec_testsuite/simd_f32x4_cmp.wast",
                        "spec_testsuite/simd_f32x4_pmin_pmax.wast",
                        "spec_testsuite/simd_f32x4_rounding.wast",
                        "spec_testsuite/simd_f64x2.wast",
                        "spec_testsuite/simd_f64x2_arith.wast",
                        "spec_testsuite/simd_f64x2_cmp.wast",
                        "spec_testsuite/simd_f64x2_pmin_pmax.wast",
                        "spec_testsuite/simd_f64x2_rounding.wast",
                        "spec_testsuite/simd_i16x8_cmp.wast",
                        "spec_testsuite/simd_i32x4_cmp.wast",
                        "spec_testsuite/simd_i64x2_arith2.wast",
                        "spec_testsuite/simd_i64x2_cmp.wast",
                        "spec_testsuite/simd_i8x16_arith2.wast",
                        "spec_testsuite/simd_i8x16_cmp.wast",
                        "spec_testsuite/simd_int_to_int_extend.wast",
                        "spec_testsuite/simd_load.wast",
                        "spec_testsuite/simd_load_extend.wast",
                        "spec_testsuite/simd_load_splat.wast",
                        "spec_testsuite/simd_load_zero.wast",
                        "spec_testsuite/simd_splat.wast",
                        "spec_testsuite/simd_store16_lane.wast",
                        "spec_testsuite/simd_store32_lane.wast",
                        "spec_testsuite/simd_store64_lane.wast",
                        "spec_testsuite/simd_store8_lane.wast",
                        "spec_testsuite/simd_load16_lane.wast",
                        "spec_testsuite/simd_load32_lane.wast",
                        "spec_testsuite/simd_load64_lane.wast",
                        "spec_testsuite/simd_load8_lane.wast",
                        "spec_testsuite/simd_bitwise.wast",
                        "misc_testsuite/simd/load_splat_out_of_bounds.wast",
                        "misc_testsuite/simd/unaligned-load.wast",
                        "multi-memory/simd_memory-multi.wast",
                        "misc_testsuite/simd/issue4807.wast",
                        "spec_testsuite/simd_const.wast",
                        "spec_testsuite/simd_i8x16_sat_arith.wast",
                        "spec_testsuite/simd_i64x2_arith.wast",
                        "spec_testsuite/simd_i16x8_arith.wast",
                        "spec_testsuite/simd_i16x8_arith2.wast",
                        "spec_testsuite/simd_i16x8_q15mulr_sat_s.wast",
                        "spec_testsuite/simd_i16x8_sat_arith.wast",
                        "spec_testsuite/simd_i32x4_arith.wast",
                        "spec_testsuite/simd_i32x4_dot_i16x8.wast",
                        "spec_testsuite/simd_i32x4_trunc_sat_f32x4.wast",
                        "spec_testsuite/simd_i32x4_trunc_sat_f64x2.wast",
                        "spec_testsuite/simd_i8x16_arith.wast",
                        "spec_testsuite/simd_bit_shift.wast",
                        "spec_testsuite/simd_lane.wast",
                        "spec_testsuite/simd_i16x8_extmul_i8x16.wast",
                        "spec_testsuite/simd_i32x4_extmul_i16x8.wast",
                        "spec_testsuite/simd_i64x2_extmul_i32x4.wast",
                        "spec_testsuite/simd_i16x8_extadd_pairwise_i8x16.wast",
                        "spec_testsuite/simd_i32x4_extadd_pairwise_i16x8.wast",
                        "spec_testsuite/simd_i32x4_arith2.wast",
                    ];

                    if unsupported.iter().any(|part| self.path.ends_with(part)) {
                        return true;
                    }
                }
            }
        }

        // Not implemented in Wasmtime anywhere yet.
        if self.config.custom_descriptors() {
            let happens_to_work =
                ["spec_testsuite/proposals/custom-descriptors/binary-leb128.wast"];

            if happens_to_work.iter().any(|part| self.path.ends_with(part)) {
                return false;
            }
            return true;
        }

        false
    }
}

fn simple_transaction_proposal_enabled(name: &str) -> bool {
    matches!(
        name,
        "float_tmemory.wast"
            | "taddress.wast"
            | "talign.wast"
            | "tblock.wast"
            | "tbr.wast"
            | "tbr_if.wast"
            | "tcall.wast"
            | "tcall_indirect.wast"
            | "return_tcall.wast"
            | "return_tcall_indirect.wast"
            | "tconst.wast"
            | "ti32.wast"
            | "ti64.wast"
            | "tf32.wast"
            | "tf32_bitwise.wast"
            | "tf32_cmp.wast"
            | "tf64.wast"
            | "tf64_bitwise.wast"
            | "tf64_cmp.wast"
            | "tconversions.wast"
            | "tendianness.wast"
            | "texports.wast"
            | "tif.wast"
            | "timports.wast"
            | "tinline-module.wast"
            | "tloop.wast"
            | "tload.wast"
            | "tlocal_get.wast"
            | "tlocal_set.wast"
            | "tlocal_tee.wast"
            | "tmemory.wast"
            | "tmemory_grow.wast"
            | "tmemory_copy.wast"
            | "tmemory_fill.wast"
            | "tmemory_init.wast"
            | "tmemory_redundancy.wast"
            | "tmemory_size.wast"
            | "tmemory_trap.wast"
            | "treturn.wast"
            | "tnop.wast"
            | "tskip-stack-guard-page.wast"
            | "tstack.wast"
            | "tstart.wast"
            | "tstore.wast"
            | "ttraps.wast"
            | "tswitch.wast"
            | "tunreachable.wast"
            | "tunwind.wast"
            | "tlabels.wast"
            | "tleft-to-right.wast"
            | "tfac.wast"
            | "tint_exprs.wast"
            | "tint_literals.wast"
            | "tfloat_exprs.wast"
            | "tfloat_misc.wast"
            | "ttype.wast"
            | "tforward.wast"
            | "tnames.wast"
            | "tutf8-invalid-encoding.wast"
            | "utf8-timport-field.wast"
            | "utf8-timport-module.wast"
            | "tfunc_ptrs.wast"
            | "tconflict-tmemory.wast"
    )
}

fn transaction_proposal_uses_real_text_parser(
    suite: TransactionProposalSuite,
    path: &Path,
) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    match suite {
        TransactionProposalSuite::SimpleTransactions => {
            matches!(name, "tmemory_size.wast" | "tmemory_grow.wast")
        }
        TransactionProposalSuite::Tsimd => false,
    }
}

fn tsimd_transaction_proposal_enabled(name: &str) -> bool {
    matches!(
        name,
        "tsimd_address.wast"
            | "tsimd_align.wast"
            | "tsimd_bit_shift.wast"
            | "tsimd_bitwise.wast"
            | "tsimd_boolean.wast"
            | "tsimd_conversions.wast"
            | "tsimd_f32x4.wast"
            | "tsimd_f32x4_arith.wast"
            | "tsimd_f32x4_cmp.wast"
            | "tsimd_f32x4_pmin_pmax.wast"
            | "tsimd_f32x4_rounding.wast"
            | "tsimd_f64x2.wast"
            | "tsimd_f64x2_arith.wast"
            | "tsimd_f64x2_cmp.wast"
            | "tsimd_f64x2_pmin_pmax.wast"
            | "tsimd_f64x2_rounding.wast"
            | "tsimd_i16x8_arith.wast"
            | "tsimd_i16x8_arith2.wast"
            | "tsimd_i16x8_cmp.wast"
            | "tsimd_i16x8_extadd_pairwise_i8x16.wast"
            | "tsimd_i16x8_extmul_i8x16.wast"
            | "tsimd_i16x8_q15mulr_sat_s.wast"
            | "tsimd_i16x8_sat_arith.wast"
            | "tsimd_i32x4_arith.wast"
            | "tsimd_i32x4_arith2.wast"
            | "tsimd_i32x4_cmp.wast"
            | "tsimd_i32x4_dot_i16x8.wast"
            | "tsimd_i32x4_extadd_pairwise_i16x8.wast"
            | "tsimd_i32x4_extmul_i16x8.wast"
            | "tsimd_i32x4_trunc_sat_f32x4.wast"
            | "tsimd_i32x4_trunc_sat_f64x2.wast"
            | "tsimd_i64x2_arith.wast"
            | "tsimd_i64x2_arith2.wast"
            | "tsimd_i64x2_cmp.wast"
            | "tsimd_i64x2_extmul_i32x4.wast"
            | "tsimd_i8x16_arith.wast"
            | "tsimd_i8x16_arith2.wast"
            | "tsimd_i8x16_cmp.wast"
            | "tsimd_i8x16_sat_arith.wast"
            | "tsimd_int_to_int_extend.wast"
            | "tsimd_lane.wast"
            | "tsimd_linking.wast"
            | "tsimd_load.wast"
            | "tsimd_load_extend.wast"
            | "tsimd_load_splat.wast"
            | "tsimd_load_zero.wast"
            | "tsimd_load8_lane.wast"
            | "tsimd_load16_lane.wast"
            | "tsimd_load32_lane.wast"
            | "tsimd_load64_lane.wast"
            | "tsimd_splat.wast"
            | "tsimd_store.wast"
            | "tsimd_store8_lane.wast"
            | "tsimd_store16_lane.wast"
            | "tsimd_store32_lane.wast"
            | "tsimd_store64_lane.wast"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        TestConfig, TransactionProposalSuite, WastTest, normalize_transaction_proposal_wast,
    };
    use std::path::PathBuf;

    #[test]
    fn normalizes_transaction_proposal_text() {
        let wast = r#"
            (module
              (type $ft (tfunc (param i32) (result i32)))
              (tmemory 1)
              (tglobal $g (mut i32) (i32.const 0))
              (tfunc $f (export "tcall(name)") (param i32) (result i32)
                (tglobal.set $g (local.get 0))
                (i32.tstore (i32.const 0) (local.get 0))
                (i32.tload (i32.const 0)))
              (func (export "run") (result i32)
                (return_tcall $f (i32.const 1))))
            (assert_return (tinvoke "run") (i32.const 1))
            (assert_trap (tinvoke "run") "out of bounds tmemory access")
            (assert_invalid (module) "unknown tmemory 0")
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains("(type $ft (func"));
        assert!(normalized.contains("(memory 1)"));
        assert!(normalized.contains("(global $g"));
        assert!(normalized.contains("(func $f"));
        assert!(normalized.contains("(global.set $g"));
        assert!(normalized.contains("(i32.store"));
        assert!(normalized.contains("(i32.load"));
        assert!(normalized.contains("(return_call $f"));
        assert!(normalized.contains("(invoke \"run\""));
        assert!(normalized.contains("(export \"tcall(name)\""));
        assert!(normalized.contains("out of bounds memory access"));
        assert!(normalized.contains("unknown memory 0"));
    }

    #[test]
    fn normalizes_only_module_quote_strings_recursively() {
        let wast = r#"
            (assert_malformed
              (module quote "(tglobal $foo i32)")
              "duplicate tglobal")
            (assert_malformed
              (module quote "(tfunc (i32.tload (i32.const 0)))")
              "diagnostic with tcall(name)")
            (assert_return (invoke "unknown tmemory 0"))
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains("\"(global $foo i32)\""));
        assert!(normalized.contains("\"duplicate tglobal\""));
        assert!(normalized.contains("\"(func (i32.load (i32.const 0)))\""));
        assert!(normalized.contains("\"diagnostic with tcall(name)\""));
        assert!(normalized.contains("\"unknown tmemory 0\""));
        assert!(!normalized.contains("\"unknown memory 0\""));
    }

    #[test]
    fn normalizes_module_quote_after_escaped_inner_strings() {
        let wast = r#"
            (assert_malformed
              (module quote "(tfunc) (import \"\" \"\" (tfunc))")
              "import after tfunction")
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains(r#""(func) (import \"\" \"\" (func))""#));
    }

    #[test]
    fn normalizes_all_module_quote_string_fragments() {
        let wast = r#"
            (assert_malformed
              (module quote
                "(type $sig (tfunc (param i32) (result i32)))"
                "(table 0 tfuncref)"
                "(tfunc (result i32)"
                "  (return_tcall_indirect (type $sig) (result i32) (param i32)"
                "    (i32.const 0) (i32.const 0)"
                "  )"
                ")")
              "unexpected token")
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains(r#""(type $sig (func (param i32) (result i32)))""#));
        assert!(normalized.contains(r#""(table 0 funcref)""#));
        assert!(normalized.contains(r#""(func (result i32)""#));
        assert!(
            normalized
                .contains(r#""  (return_call_indirect (type $sig) (result i32) (param i32)""#)
        );
    }

    #[test]
    fn normalizes_transaction_spectest_import_field_names() {
        let wast = r#"
            (module
              (tfunc $print_i32 (import "spectest" "tprint_i32") (param i32))
              (tfunc $print (import "spectest" "tprint"))
              (tfunc (export "tprint_i32") (tcall $print_i32 (i32.const 1))))
            (assert_return (tinvoke "tprint"))
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains(r#"(func $print_i32 (import "spectest" "print_i32")"#));
        assert!(normalized.contains(r#"(func $print (import "spectest" "print")"#));
        assert!(normalized.contains(r#"(export "tprint_i32")"#));
        assert!(normalized.contains(r#"(invoke "tprint")"#));
    }

    #[test]
    fn normalizes_only_spectest_import_field_names() {
        let wast = r#"
            (module
              (func (import "tprint_i32" "tprint_i32"))
              (func (import "spectest" "tmemory"))
              (func (import "spectest" "ttable"))
              (func (import "spectest" "tprint_i32"))
              (func (import "spectest" "tprint_i32_f32"))
              (func (import "spectest" "tprint_f64_f64")))
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains(r#"(func (import "tprint_i32" "tprint_i32"))"#));
        assert!(normalized.contains(r#"(func (import "spectest" "memory"))"#));
        assert!(normalized.contains(r#"(func (import "spectest" "table"))"#));
        assert!(normalized.contains(r#"(func (import "spectest" "print_i32"))"#));
        assert!(normalized.contains(r#"(func (import "spectest" "print_i32_f32"))"#));
        assert!(normalized.contains(r#"(func (import "spectest" "print_f64_f64"))"#));
    }

    #[test]
    fn normalizes_transaction_memory_diagnostics() {
        let wast = r#"
            (assert_invalid (module (tmemory 0) (tmemory 0)) "multiple tmemories")
            (assert_invalid (module (tmemory 65537)) "memory size must be at most 65536 pages (4GiB)")
            (assert_return (invoke "multiple tmemories"))
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains("\"multiple memories\""));
        assert!(normalized.contains("\"memory size must be at most 0x10000 65536-byte pages\""));
        assert!(normalized.contains("\"multiple tmemories\""));
    }

    #[test]
    fn normalizes_transaction_bulk_memory_tokens() {
        let wast = r#"
            (module
              (tmemory 1)
              (tdata "abc")
              (tfunc
                (tmemory.copy (i32.const 0) (i32.const 1) (i32.const 2))
                (tmemory.fill (i32.const 0) (i32.const 1) (i32.const 2))
                (tmemory.init 0 (i32.const 0) (i32.const 1) (i32.const 2))
                (tdata.drop 0)))
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains("(memory.copy"));
        assert!(normalized.contains("(memory.fill"));
        assert!(normalized.contains("(memory.init 0"));
        assert!(normalized.contains("(data.drop 0"));
    }

    #[test]
    fn normalizes_transaction_table_and_get_tokens() {
        let wast = r#"
            (module
              (table 1 funcref)
              (func
                (drop (ttable.size))
                (drop (ttable.grow (ref.null func) (i32.const 0)))
                (ttable.fill (i32.const 0) (ref.null func) (i32.const 0))
                (ttable.copy 0 0 (i32.const 0) (i32.const 0) (i32.const 0))
                (ttable.init 0 (i32.const 0) (i32.const 0) (i32.const 0))
                (telem.drop 0))
              (export "x" (ttable 0)))
            (assert_return (tget "x"))
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains("(table.size"));
        assert!(normalized.contains("(table.grow"));
        assert!(normalized.contains("(table.fill"));
        assert!(normalized.contains("(table.copy"));
        assert!(normalized.contains("(table.init"));
        assert!(normalized.contains("(elem.drop 0"));
        assert!(normalized.contains(r#"(export "x" (table 0))"#));
        assert!(normalized.contains(r#"(assert_return (get "x")"#));
    }

    #[test]
    fn normalizes_transaction_simd_memory_tokens() {
        let wast = r#"
            (module
              (tmemory 1)
              (tfunc
                (v128.tstore (i32.const 0) (v128.tload (i32.const 0)))
                (drop (v128.tload8_splat (i32.const 0)))
                (drop (v128.tload16x4_u (i32.const 0)))
                (drop (v128.tload32_zero (i32.const 0)))
                (drop (v128.tload8_lane 0 (i32.const 0) (v128.const i32x4 0 0 0 0)))
                (v128.tstore64_lane 0 (i32.const 0) (v128.const i32x4 0 0 0 0))))
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains("(v128.store "));
        assert!(normalized.contains("(v128.load "));
        assert!(normalized.contains("(v128.load8_splat "));
        assert!(normalized.contains("(v128.load16x4_u "));
        assert!(normalized.contains("(v128.load32_zero "));
        assert!(normalized.contains("(v128.load8_lane "));
        assert!(normalized.contains("(v128.store64_lane "));
    }

    #[test]
    fn normalizes_transaction_simd_diagnostics() {
        let wast = r#"
            (assert_invalid
              (module (tfunc (result v128)
                (v128.tload8_lane 16 (i32.const 0) (v128.const i32x4 0 0 0 0))))
              "invalid lane index")
        "#;

        let normalized = normalize_transaction_proposal_wast(wast);

        assert!(normalized.contains("\"SIMD index out of bounds\""));
    }

    #[test]
    fn enables_normalized_core_transaction_proposal_tranche() {
        for name in ["tstack.wast", "tstart.wast", "tswitch.wast", "tunwind.wast"] {
            let test = WastTest {
                path: PathBuf::from(name),
                contents: String::new(),
                config: TestConfig::default(),
                transaction_proposal: Some(TransactionProposalSuite::SimpleTransactions),
                transaction_real_text_parser: false,
            };

            assert!(test.transaction_proposal_enabled(), "{name}");
        }
    }

    #[test]
    fn enables_normalized_memory_transaction_proposal_tranche() {
        for name in [
            "float_tmemory.wast",
            "taddress.wast",
            "talign.wast",
            "tendianness.wast",
            "tload.wast",
            "tmemory.wast",
            "tmemory_redundancy.wast",
            "tmemory_trap.wast",
            "tskip-stack-guard-page.wast",
            "tstore.wast",
        ] {
            let test = WastTest {
                path: PathBuf::from(name),
                contents: String::new(),
                config: TestConfig::default(),
                transaction_proposal: Some(TransactionProposalSuite::SimpleTransactions),
                transaction_real_text_parser: false,
            };

            assert!(test.transaction_proposal_enabled(), "{name}");
        }
    }

    #[test]
    fn real_text_parser_transaction_proposal_tranche_is_not_normalized_or_run_yet() {
        for name in ["tmemory_size.wast", "tmemory_grow.wast"] {
            let test = WastTest {
                path: PathBuf::from(name),
                contents: String::new(),
                config: TestConfig::default(),
                transaction_proposal: Some(TransactionProposalSuite::SimpleTransactions),
                transaction_real_text_parser: true,
            };

            assert!(test.transaction_real_text_parser(), "{name}");
            assert!(!test.transaction_proposal_enabled(), "{name}");
            assert!(super::transaction_proposal_uses_real_text_parser(
                TransactionProposalSuite::SimpleTransactions,
                &test.path
            ));
        }
    }

    #[test]
    fn enables_normalized_bulk_memory_transaction_proposal_tranche() {
        for name in [
            "tmemory_copy.wast",
            "tmemory_fill.wast",
            "tmemory_init.wast",
        ] {
            let test = WastTest {
                path: PathBuf::from(name),
                contents: String::new(),
                config: TestConfig::default(),
                transaction_proposal: Some(TransactionProposalSuite::SimpleTransactions),
                transaction_real_text_parser: false,
            };

            assert!(test.transaction_proposal_enabled(), "{name}");
        }
    }

    #[test]
    fn enables_normalized_import_export_transaction_proposal_tranche() {
        for name in ["texports.wast", "timports.wast", "tinline-module.wast"] {
            let test = WastTest {
                path: PathBuf::from(name),
                contents: String::new(),
                config: TestConfig::default(),
                transaction_proposal: Some(TransactionProposalSuite::SimpleTransactions),
                transaction_real_text_parser: false,
            };

            assert!(test.transaction_proposal_enabled(), "{name}");
        }
    }

    #[test]
    fn enables_normalized_type_and_name_transaction_proposal_tranche() {
        for name in [
            "ttype.wast",
            "tforward.wast",
            "tnames.wast",
            "tutf8-invalid-encoding.wast",
            "utf8-timport-field.wast",
            "utf8-timport-module.wast",
            "tfunc_ptrs.wast",
        ] {
            let test = WastTest {
                path: PathBuf::from(name),
                contents: String::new(),
                config: TestConfig::default(),
                transaction_proposal: Some(TransactionProposalSuite::SimpleTransactions),
                transaction_real_text_parser: false,
            };

            assert!(test.transaction_proposal_enabled(), "{name}");
        }
    }

    #[test]
    fn enables_normalized_conflict_transaction_proposal_tranche() {
        for name in ["tconflict-tmemory.wast"] {
            let test = WastTest {
                path: PathBuf::from(name),
                contents: String::new(),
                config: TestConfig::default(),
                transaction_proposal: Some(TransactionProposalSuite::SimpleTransactions),
                transaction_real_text_parser: false,
            };

            assert!(test.transaction_proposal_enabled(), "{name}");
        }
    }

    #[test]
    fn enables_normalized_transaction_simd_proposal_tranche() {
        for name in [
            "tsimd_address.wast",
            "tsimd_align.wast",
            "tsimd_bit_shift.wast",
            "tsimd_bitwise.wast",
            "tsimd_boolean.wast",
            "tsimd_conversions.wast",
            "tsimd_f32x4.wast",
            "tsimd_f32x4_arith.wast",
            "tsimd_f32x4_cmp.wast",
            "tsimd_f32x4_pmin_pmax.wast",
            "tsimd_f32x4_rounding.wast",
            "tsimd_f64x2.wast",
            "tsimd_f64x2_arith.wast",
            "tsimd_f64x2_cmp.wast",
            "tsimd_f64x2_pmin_pmax.wast",
            "tsimd_f64x2_rounding.wast",
            "tsimd_i16x8_arith.wast",
            "tsimd_i16x8_arith2.wast",
            "tsimd_i16x8_cmp.wast",
            "tsimd_i16x8_extadd_pairwise_i8x16.wast",
            "tsimd_i16x8_extmul_i8x16.wast",
            "tsimd_i16x8_q15mulr_sat_s.wast",
            "tsimd_i16x8_sat_arith.wast",
            "tsimd_i32x4_arith.wast",
            "tsimd_i32x4_arith2.wast",
            "tsimd_i32x4_cmp.wast",
            "tsimd_i32x4_dot_i16x8.wast",
            "tsimd_i32x4_extadd_pairwise_i16x8.wast",
            "tsimd_i32x4_extmul_i16x8.wast",
            "tsimd_i32x4_trunc_sat_f32x4.wast",
            "tsimd_i32x4_trunc_sat_f64x2.wast",
            "tsimd_i64x2_arith.wast",
            "tsimd_i64x2_arith2.wast",
            "tsimd_i64x2_cmp.wast",
            "tsimd_i64x2_extmul_i32x4.wast",
            "tsimd_i8x16_arith.wast",
            "tsimd_i8x16_arith2.wast",
            "tsimd_i8x16_cmp.wast",
            "tsimd_i8x16_sat_arith.wast",
            "tsimd_int_to_int_extend.wast",
            "tsimd_lane.wast",
            "tsimd_linking.wast",
            "tsimd_load.wast",
            "tsimd_load_extend.wast",
            "tsimd_load_splat.wast",
            "tsimd_load_zero.wast",
            "tsimd_load8_lane.wast",
            "tsimd_load16_lane.wast",
            "tsimd_load32_lane.wast",
            "tsimd_load64_lane.wast",
            "tsimd_splat.wast",
            "tsimd_store.wast",
            "tsimd_store8_lane.wast",
            "tsimd_store16_lane.wast",
            "tsimd_store32_lane.wast",
            "tsimd_store64_lane.wast",
        ] {
            let test = WastTest {
                path: PathBuf::from(name),
                contents: String::new(),
                config: TestConfig::default(),
                transaction_proposal: Some(TransactionProposalSuite::Tsimd),
                transaction_real_text_parser: false,
            };

            assert!(test.transaction_proposal_enabled(), "{name}");
        }
    }
}

fn spec_proposal_from_path(path: &Path) -> Option<&str> {
    let mut iter = path.iter();
    loop {
        match iter.next()?.to_str()? {
            "proposals" => break,
            _ => {}
        }
    }
    Some(iter.next()?.to_str()?)
}
