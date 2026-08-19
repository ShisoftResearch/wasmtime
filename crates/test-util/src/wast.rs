use serde::de::DeserializeOwned;
use serde_derive::Deserialize;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
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
#[derive(Clone, Debug)]
pub struct TestDiscoveryConfig {
    pub transaction_proposal: bool,
    /// Optional `test/core` root of the transaction proposal checkout.
    pub transaction_proposal_root: Option<PathBuf>,
}

impl Default for TestDiscoveryConfig {
    fn default() -> Self {
        Self {
            transaction_proposal: cfg!(feature = "transaction")
                && std::env::var_os("WASMTIME_TEST_TRANSACTION_WAST").is_some(),
            transaction_proposal_root: std::env::var_os("WASMTIME_TEST_TRANSACTION_WAST_ROOT")
                .map(PathBuf::from),
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
        match discovery.transaction_proposal_root {
            Some(proposal_root) => {
                add_transaction_proposal_tests(&mut tests, &proposal_root)?;
            }
            None => {
                let proposal_root = root.join("../wasm-persistence/test/core");
                add_git_tracked_transaction_proposal_tests(&mut tests, &proposal_root)?;
            }
        }
    }

    Ok(tests)
}

fn add_transaction_proposal_tests(tests: &mut Vec<WastTest>, proposal_root: &Path) -> Result<()> {
    for (suite, dir) in [
        (
            TransactionProposalSuite::SimpleTransactions,
            "simple-transactions",
        ),
        (TransactionProposalSuite::Tsimd, "tsimd"),
    ] {
        let path = proposal_root.join(dir);
        add_tests(tests, &path, &FindConfig::TransactionProposal(suite)).with_context(|| {
            format!(
                "failed to add transactional Wasm proposal tests from `{}`",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn add_git_tracked_transaction_proposal_tests(
    tests: &mut Vec<WastTest>,
    proposal_root: &Path,
) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(proposal_root)
        .args(["ls-files", "-z", "--", "simple-transactions", "tsimd"])
        .output()
        .with_context(|| {
            format!(
                "failed to query git-tracked transaction proposal tests under `{}`",
                proposal_root.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "failed to query git-tracked transaction proposal tests under `{}`: {}",
            proposal_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let paths = std::str::from_utf8(&output.stdout)
        .context("git returned a non-UTF-8 transaction proposal path")?;
    let mut found_simple_transactions = false;
    let mut found_tsimd = false;
    for relative in paths.split_terminator('\0') {
        let relative = Path::new(relative);
        let suite = match relative.components().next().and_then(|component| {
            let component = component.as_os_str();
            if component == "simple-transactions" {
                Some(TransactionProposalSuite::SimpleTransactions)
            } else if component == "tsimd" {
                Some(TransactionProposalSuite::Tsimd)
            } else {
                None
            }
        }) {
            Some(suite) => suite,
            None => bail!(
                "git returned transaction proposal path outside an authoritative suite: `{}`",
                relative.display()
            ),
        };
        if relative
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("wast")
        {
            continue;
        }
        match suite {
            TransactionProposalSuite::SimpleTransactions => found_simple_transactions = true,
            TransactionProposalSuite::Tsimd => found_tsimd = true,
        }
        add_test(
            tests,
            &proposal_root.join(relative),
            &FindConfig::TransactionProposal(suite),
        )?;
    }
    ensure!(
        found_simple_transactions,
        "git-tracked transaction proposal corpus has no simple-transactions WAST files"
    );
    ensure!(
        found_tsimd,
        "git-tracked transaction proposal corpus has no tsimd WAST files"
    );
    Ok(())
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

        add_test(tests, &path, config)?;
    }
    Ok(())
}

fn add_test(tests: &mut Vec<WastTest>, path: &Path, config: &FindConfig) -> Result<()> {
    // These tests use `*.wast` directives not yet supported by Wasmtime, so
    // wait for a `wasm-tools` update to ungate these.
    if path.ends_with("spec_testsuite/custom/custom_annot.wast")
        || path.ends_with("spec_testsuite/custom/branch_hint.wast")
        || path.ends_with("spec_testsuite/custom/name_annot.wast")
    {
        return Ok(());
    }

    let mut contents =
        fs::read_to_string(path).with_context(|| format!("failed to read test: {path:?}"))?;
    let test_config = match config {
        FindConfig::InTest => parse_test_config(&contents, ";;!")
            .with_context(|| format!("failed to parse test configuration: {path:?}"))?,
        FindConfig::Infer(f) => f(path),
        FindConfig::TransactionProposal(suite) => transaction_proposal_test_config(*suite),
    };
    let transaction_proposal = match config {
        FindConfig::TransactionProposal(suite) => Some(*suite),
        _ => None,
    };
    let transaction_real_text_parser = transaction_proposal
        .is_some_and(|suite| transaction_proposal_uses_real_text_parser(suite, path));
    if transaction_proposal.is_some() && transaction_real_text_parser {
        contents = normalize_transaction_proposal_wast_diagnostics(&contents);
    }
    tests.push(WastTest {
        path: path.to_owned(),
        contents,
        config: test_config,
        transaction_proposal,
        transaction_real_text_parser,
    });
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

fn transaction_proposal_test_config(suite: TransactionProposalSuite) -> TestConfig {
    let mut ret = TestConfig::default();
    ret.bulk_memory = Some(true);
    ret.gc = Some(true);
    ret.reference_types = Some(true);
    ret.function_references = Some(true);
    ret.tail_call = Some(true);

    ret.simd = Some(matches!(
        suite,
        TransactionProposalSuite::SimpleTransactions | TransactionProposalSuite::Tsimd
    ));

    ret
}

// Proposal WAST assertions use proposal diagnostic wording. Wasmtime is allowed
// to use equivalent local diagnostics, so keep this pass scoped to assertion
// strings and nested `(module quote "...")` strings only.
fn normalize_transaction_proposal_wast_diagnostics(wast: &str) -> String {
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
                let diagnostic = lists.last().is_some_and(ListContext::is_assert_diagnostic);
                out.push_str(&normalize_transaction_string(
                    &wast[idx..end],
                    quote_module,
                    diagnostic,
                ));
                if let Some(tokens) = lists.last_mut() {
                    if let Some(body) = wast[idx..end]
                        .strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                    {
                        tokens.tokens.push(body.to_string());
                    }
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
                out.push_str(token);
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
}

impl ListContext {
    fn is_assert_diagnostic(&self) -> bool {
        self.tokens.first().is_some_and(|token| {
            matches!(
                token.as_str(),
                "assert_invalid" | "assert_malformed" | "assert_trap" | "assert_exhaustion"
            )
        })
    }
}

fn normalize_transaction_string(string: &str, quote_module: bool, diagnostic: bool) -> String {
    let Some(body) = string.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return string.to_string();
    };

    if quote_module {
        let decoded = decode_module_quote_body(body);
        let normalized = normalize_transaction_proposal_wast_diagnostics(&decoded);
        return format!("\"{}\"", encode_module_quote_body(&normalized));
    }

    if diagnostic {
        let normalized = normalize_transaction_real_parser_diagnostic(body);
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

fn normalize_transaction_real_parser_diagnostic(text: &str) -> String {
    if text == "null tarray reference" {
        return text.into();
    }
    if text.starts_with("type mismatch: instruction requires ") {
        return "type mismatch".into();
    }
    apply_transaction_diagnostic_replacements(text, TRANSACTION_SHARED_DIAGNOSTIC_REPLACEMENTS)
}

const TRANSACTION_SHARED_DIAGNOSTIC_REPLACEMENTS: &[(&str, &str)] = &[
    ("tarray is immutable", "array is immutable"),
    ("tarray types do not match", "array types do not match"),
    (
        "tarray type is not numeric or vector",
        "array type is not numeric or vector",
    ),
    ("immutable tglobal", "global is immutable"),
    ("out of bounds ttable access", "out of bounds table access"),
    ("out of bounds tarray access", "out of bounds array access"),
    ("undefined telement", "undefined element"),
    ("uninitialized telement", "uninitialized element"),
    ("null tstructure", "null reference"),
    ("null tarray", "null reference"),
    (
        "indirect tcall type mismatch",
        "indirect call type mismatch",
    ),
    ("indirect tcall", "indirect call type mismatch"),
    (
        "memory size must be at most 65536 pages (4GiB)",
        "memory size must be at most 0x10000 65536-byte pages",
    ),
    ("multiple tmemories", "multiple memories"),
    ("unknown tmemory", "unknown memory"),
    ("unknown tdata segment", "unknown data segment"),
    ("inline tfunction type", "inline function type"),
    ("null tfunction", "null function"),
    ("unknown tglobal", "unknown global"),
    ("invalid lane index", "SIMD index out of bounds"),
];

fn apply_transaction_diagnostic_replacements(
    text: impl Into<String>,
    replacements: &[(&str, &str)],
) -> String {
    replacements
        .iter()
        .fold(text.into(), |text, (from, to)| text.replace(from, to))
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
    /// and diagnostic-only assertion rewriting.
    pub fn transaction_real_text_parser(&self) -> bool {
        self.transaction_real_text_parser
    }

    /// Returns whether this transactional proposal test can currently run.
    pub fn transaction_proposal_enabled(&self) -> bool {
        self.transaction_proposal.is_some()
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

fn transaction_proposal_uses_real_text_parser(
    suite: TransactionProposalSuite,
    _path: &Path,
) -> bool {
    matches!(
        suite,
        TransactionProposalSuite::SimpleTransactions | TransactionProposalSuite::Tsimd
    )
}

#[cfg(test)]
mod tests {
    use super::{TestDiscoveryConfig, TransactionProposalSuite, transaction_proposal_test_config};
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempDir {
        path: PathBuf,
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn temp_transaction_dir(label: &str) -> TempDir {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        TempDir {
            path: std::env::temp_dir()
                .join(format!("wasmtime-{label}-{unique}-{}", std::process::id())),
        }
    }

    #[test]
    fn discovers_every_transaction_proposal_fixture_as_enabled_real_text() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let proposal_root = repo.join("../wasm-persistence/test/core");
        let mut found_tfunc_block = false;
        let mut found_simple_transactions = false;
        let mut found_tsimd = false;
        let mut tests = Vec::new();
        super::add_git_tracked_transaction_proposal_tests(&mut tests, &proposal_root).unwrap();

        assert!(!tests.is_empty(), "no tracked proposal fixtures discovered");
        for test in tests {
            let name = test.path.file_name().unwrap().to_string_lossy();
            assert!(
                test.transaction_proposal_enabled(),
                "{} was discovered but disabled",
                test.path.display()
            );
            assert!(
                test.transaction_real_text_parser(),
                "{} did not use the native text parser",
                test.path.display()
            );
            match test.transaction_proposal {
                Some(TransactionProposalSuite::SimpleTransactions) => {
                    found_simple_transactions = true;
                    found_tfunc_block |= name.as_ref() == "tfunc_block.wast";
                }
                Some(TransactionProposalSuite::Tsimd) => found_tsimd = true,
                None => panic!("{} lost its proposal suite", test.path.display()),
            }
        }

        assert!(
            found_simple_transactions,
            "no simple-transactions discovered"
        );
        assert!(found_tsimd, "no tsimd fixtures discovered");
        assert!(found_tfunc_block, "tfunc_block.wast was not discovered");
    }

    #[test]
    fn transaction_proposal_discovery_accepts_an_explicit_corpus_root() {
        let root = temp_transaction_dir("explicit-corpus-root");
        let simple = root.path.join("simple-transactions");
        let tsimd = root.path.join("tsimd");
        fs::create_dir_all(&simple).unwrap();
        fs::create_dir_all(&tsimd).unwrap();
        fs::write(simple.join("tfunc_block.wast"), "(module (tfunc))").unwrap();
        fs::write(tsimd.join("tv128.wast"), "(module (tfunc))").unwrap();

        let mut tests = Vec::new();
        super::add_transaction_proposal_tests(&mut tests, &root.path).unwrap();
        tests.sort_by(|left, right| left.path.cmp(&right.path));

        assert_eq!(tests.len(), 2);
        assert!(tests.iter().all(|test| test.transaction_proposal_enabled()));
        assert!(tests.iter().all(|test| test.transaction_real_text_parser()));
        assert_eq!(
            tests
                .iter()
                .map(|test| test.path.file_name().unwrap().to_string_lossy())
                .collect::<Vec<_>>(),
            ["tfunc_block.wast", "tv128.wast"]
        );
    }

    #[test]
    fn full_discovery_keeps_every_transaction_proposal_filename() {
        let temp = temp_transaction_dir("full-proposal-discovery");
        let repository_root = temp.path.join("repository");
        let proposal_root = temp.path.join("proposal");

        for path in [
            repository_root.join("tests/spec_testsuite"),
            repository_root.join("tests/misc_testsuite"),
            repository_root.join("tests/component-model/test"),
            proposal_root.join("simple-transactions"),
            proposal_root.join("tsimd"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(
            proposal_root.join("simple-transactions/must-not-be-skipped.wast"),
            "(module (tfunc))",
        )
        .unwrap();
        fs::write(
            proposal_root.join("tsimd/same-basename-is-still-discovered.wast"),
            "(module (tfunc))",
        )
        .unwrap();

        let mut tests = super::find_tests_with_config(
            &repository_root,
            TestDiscoveryConfig {
                transaction_proposal: true,
                transaction_proposal_root: Some(proposal_root),
            },
        )
        .unwrap();
        tests.retain(|test| test.transaction_proposal_enabled());
        tests.sort_by(|left, right| left.path.cmp(&right.path));

        assert_eq!(
            tests
                .iter()
                .map(|test| test.path.file_name().unwrap().to_string_lossy())
                .collect::<Vec<_>>(),
            [
                "must-not-be-skipped.wast",
                "same-basename-is-still-discovered.wast"
            ]
        );
    }

    #[test]
    fn default_transaction_proposal_discovery_uses_only_git_tracked_files() {
        let checkout = temp_transaction_dir("tracked-corpus-root");
        let proposal_root = checkout.path.join("test/core");
        let simple = proposal_root.join("simple-transactions");
        let tsimd = proposal_root.join("tsimd/nested");
        fs::create_dir_all(&simple).unwrap();
        fs::create_dir_all(&tsimd).unwrap();
        fs::write(simple.join("tracked.wast"), "(module)").unwrap();
        fs::write(simple.join("untracked.wast"), "(module)").unwrap();
        fs::write(tsimd.join("tracked-simd.wast"), "(module)").unwrap();

        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&checkout.path)
                .args(["init", "--quiet"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&checkout.path)
                .args([
                    "add",
                    "test/core/simple-transactions/tracked.wast",
                    "test/core/tsimd/nested/tracked-simd.wast",
                ])
                .status()
                .unwrap()
                .success()
        );

        let mut tests = Vec::new();
        super::add_git_tracked_transaction_proposal_tests(&mut tests, &proposal_root).unwrap();
        tests.sort_by(|left, right| left.path.cmp(&right.path));

        assert_eq!(
            tests
                .iter()
                .map(|test| test.path.file_name().unwrap().to_string_lossy())
                .collect::<Vec<_>>(),
            ["tracked.wast", "tracked-simd.wast"]
        );
    }

    #[test]
    fn default_transaction_proposal_discovery_rejects_an_incomplete_tracked_corpus() {
        let checkout = temp_transaction_dir("incomplete-tracked-corpus");
        let proposal_root = checkout.path.join("test/core");
        let simple = proposal_root.join("simple-transactions");
        let tsimd = proposal_root.join("tsimd");
        fs::create_dir_all(&simple).unwrap();
        fs::create_dir_all(&tsimd).unwrap();
        fs::write(simple.join("tracked.wast"), "(module)").unwrap();

        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&checkout.path)
                .args(["init", "--quiet"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&checkout.path)
                .args(["add", "test/core/simple-transactions/tracked.wast"])
                .status()
                .unwrap()
                .success()
        );

        let error =
            super::add_git_tracked_transaction_proposal_tests(&mut Vec::new(), &proposal_root)
                .unwrap_err();
        assert!(error.to_string().contains("tsimd"), "{error:#}");
    }

    #[test]
    fn transaction_feature_is_default_enabled_on_research_branch() {
        assert!(
            cfg!(feature = "transaction"),
            "the transaction branch keeps the transaction feature default-on"
        );
    }

    #[test]
    fn normalizes_real_parser_transaction_memory_diagnostics() {
        let wast = r#"
            (assert_invalid (module (tmemory 0) (tmemory 0)) "multiple tmemories")
            (assert_invalid (module (tfunc (drop (tmemory.size)))) "unknown tmemory")
            (assert_invalid (module (tfunc (tdata.drop 0))) "unknown tdata segment")
            (assert_return (invoke "multiple tmemories"))
        "#;

        let normalized = super::normalize_transaction_proposal_wast_diagnostics(wast);

        assert!(normalized.contains("\"multiple memories\""));
        assert!(normalized.contains("\"unknown memory\""));
        assert!(normalized.contains("\"unknown data segment\""));
        assert!(normalized.contains("\"multiple tmemories\""));
    }

    #[test]
    fn normalizes_real_parser_transaction_object_diagnostics() {
        let wast = r#"
            (assert_invalid (module) "tarray is immutable")
            (assert_invalid (module) "tarray types do not match")
            (assert_invalid (module) "tarray type is not numeric or vector")
            (assert_invalid (module) "immutable tglobal")
            (assert_trap (invoke "x") "null tstructure")
            (assert_trap (invoke "x") "null tarray")
            (assert_trap (invoke "x") "out of bounds tarray access")
            (assert_trap (invoke "x") "indirect tcall type mismatch")
            (assert_trap (invoke "x") "indirect tcall")
        "#;

        let normalized = super::normalize_transaction_proposal_wast_diagnostics(wast);

        assert!(normalized.contains("\"array is immutable\""));
        assert!(normalized.contains("\"array types do not match\""));
        assert!(normalized.contains("\"array type is not numeric or vector\""));
        assert!(normalized.contains("\"global is immutable\""));
        assert!(normalized.contains("\"null reference\""));
        assert!(normalized.contains("\"out of bounds array access\""));
        assert!(normalized.contains("\"indirect call type mismatch\""));
        assert!(!normalized.contains("\"indirect call type mismatch type mismatch\""));
    }

    #[test]
    fn normalizes_real_parser_transaction_permission_diagnostics() {
        let wast = r#"
            (assert_invalid
              (module)
              "type mismatch: instruction requires [(tref null write (tarray (mut i8))) i32 (tref null read (tarray (mut i8))) i32 i32] but stack has [(tref 0) i32 (tref 0) i32 i32]")
        "#;

        let normalized = super::normalize_transaction_proposal_wast_diagnostics(wast);

        assert!(normalized.contains("\"type mismatch\""));
        assert!(!normalized.contains("instruction requires"));
    }

    #[test]
    fn transaction_proposal_suites_enable_simd_by_directory() {
        assert!(
            transaction_proposal_test_config(TransactionProposalSuite::SimpleTransactions).simd()
        );
        assert!(transaction_proposal_test_config(TransactionProposalSuite::Tsimd).simd());
    }

    #[test]
    fn enables_transaction_proposal_ttry_basic_with_real_parser() {
        let root = temp_transaction_dir("ttry-basic");
        let dir = root.path.join("simple-transactions");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ttry-basic.wast");
        fs::write(
            &path,
            r#"(module
  (tfunc $maybe-fail (param i32 i32)
    (if (local.get 0) (then (tfail (local.get 1))))
  )

  (func (export "try2") (param i32 i32 i32 i32) (result i32)
    (local i32 i32)
    (local.set 4 (i32.const 0))
    (local.set 5 (i32.const 0))
    (ttry ((tcall $maybe-fail (local.get 0) (local.get 1))
           (local.set 4 (i32.const 1))
           (tcall $maybe-fail (local.get 2) (local.get 3))
           (local.set 4 (i32.const 2)))
        (else (local.set 5)))
    (i32.add (local.get 4) (local.get 5))
  )
)

(assert_return (invoke "try2" (i32.const 0) (i32.const 10) (i32.const 0) (i32.const 20)) (i32.const 2))
(assert_return (invoke "try2" (i32.const 1) (i32.const 10) (i32.const 0) (i32.const 20)) (i32.const 10))
(assert_return (invoke "try2" (i32.const 0) (i32.const 10) (i32.const 1) (i32.const 20)) (i32.const 21))
"#,
        )
        .unwrap();

        let mut tests = Vec::new();
        super::add_tests(
            &mut tests,
            &root.path,
            &super::FindConfig::TransactionProposal(TransactionProposalSuite::SimpleTransactions),
        )
        .unwrap();

        let test = tests.into_iter().next().unwrap();
        assert!(test.transaction_proposal_enabled());
        assert!(test.transaction_real_text_parser());
        assert!(test.contents.contains(r#"(func (export "try2")"#));
        assert!(test.contents.contains(
            r#"(assert_return (invoke "try2" (i32.const 0) (i32.const 10) (i32.const 0) (i32.const 20)) (i32.const 2))"#
        ));
        assert!(test.contents.contains(
            r#"(assert_return (invoke "try2" (i32.const 1) (i32.const 10) (i32.const 0) (i32.const 20)) (i32.const 10))"#
        ));
        assert!(test.contents.contains(
            r#"(assert_return (invoke "try2" (i32.const 0) (i32.const 10) (i32.const 1) (i32.const 20)) (i32.const 21))"#
        ));
        assert!(test.contents.contains("(ttry"));
        assert!(test.contents.contains("(tfail"));
    }

    #[test]
    fn enables_transaction_proposal_tcall_ref_real_parser_files() {
        for (name, source, required) in [
            (
                "tcall_ref.wast",
                r#"(module
  (type $ii (tfunc (param i32) (result i32)))
  (tfunc (export "run") (param i32) (result i32)
    (tcall_ref $ii (local.get 0) (tref.null $ii)))
)

(assert_return (tinvoke "run" (i32.const 0)) (i32.const 0))
(assert_trap (tinvoke "null") "null tfunction")
(assert_invalid (module (type $t (tfunc)) (tfunc $f (param $r texterntref) (tcall_ref $t (local.get $r)))) "type mismatch")
"#,
                "(tcall_ref",
            ),
            (
                "return_tcall_ref.wast",
                r#"(module
  (type $proc (tfunc))
  (type $-i32 (tfunc (result i32)))
  (tfunc (export "type-i32") (result i32)
    (return_tcall_ref $-i32 (tref.null $proc)))
)

(assert_return (tinvoke "type-i32") (i32.const 0x132))
(assert_trap (tinvoke "null") "null tfunction")
(assert_invalid (module (type $t (tfunc)) (tfunc $f (param $r externref) (return_tcall_ref $t (local.get $r)))) "type mismatch")
"#,
                "(return_tcall_ref",
            ),
        ] {
            let root = temp_transaction_dir(name.trim_end_matches(".wast"));
            let dir = root.path.join("simple-transactions");
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            fs::write(&path, source).unwrap();

            let mut tests = Vec::new();
            super::add_tests(
                &mut tests,
                &root.path,
                &super::FindConfig::TransactionProposal(
                    TransactionProposalSuite::SimpleTransactions,
                ),
            )
            .unwrap();

            let test = tests.into_iter().next().unwrap();
            assert!(test.transaction_proposal_enabled(), "{name}");
            assert!(test.transaction_real_text_parser(), "{name}");
            assert!(test.contents.contains(required), "{name}");
            assert!(test.contents.contains("(tref.null"), "{name}");
        }
    }

    #[test]
    fn enables_transaction_proposal_ref_control_real_parser_files() {
        for (name, source, required) in [
            (
                "br_on_tnon_null.wast",
                r#"(module
  (type $t (tfunc (result i32)))
  (tfunc (export "nullable-null") (result i32)
    (br_on_tnon_null 0 (tref.null $t)))
)
(assert_return (tinvoke "nullable-null") (i32.const -1))
"#,
                "(br_on_tnon_null",
            ),
            (
                "br_on_tnull.wast",
                r#"(module
  (type $t (tfunc (result i32)))
  (tfunc (export "nullable-null") (result i32)
    (br_on_tnull 0 (tref.null $t)))
)
(assert_return (tinvoke "nullable-null") (i32.const -1))
"#,
                "(br_on_tnull",
            ),
            (
                "tref_as_non_null.wast",
                r#"(module
  (type $t (tfunc (result i32)))
  (tfunc (export "nullable-null") (result i32)
    (tref.as_non_null (tref.null $t)))
)
(assert_trap (tinvoke "nullable-null") "null treference")
"#,
                "(tref.as_non_null",
            ),
        ] {
            let root = temp_transaction_dir(name.trim_end_matches(".wast"));
            let dir = root.path.join("simple-transactions");
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            fs::write(&path, source).unwrap();

            let mut tests = Vec::new();
            super::add_tests(
                &mut tests,
                &root.path,
                &super::FindConfig::TransactionProposal(
                    TransactionProposalSuite::SimpleTransactions,
                ),
            )
            .unwrap();

            let test = tests.into_iter().next().unwrap();
            assert!(test.transaction_proposal_enabled(), "{name}");
            assert!(test.transaction_real_text_parser(), "{name}");
            assert!(test.contents.contains(required), "{name}");
            assert!(test.contents.contains("(tref.null"), "{name}");
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
