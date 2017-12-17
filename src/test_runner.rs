//-
// Copyright 2017 Jason Lingle
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! State and functions for running proptest tests.
//!
//! You do not normally need to access things in this module directly except
//! when implementing new low-level strategies.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

use rand::{self, Rand, SeedableRng, XorShiftRng};

use strategy::*;

/// The default config, computed by combining environment variables and
/// defaults.
lazy_static! {
    static ref DEFAULT_CONFIG: Config = {
        let mut result = Config {
            cases: 256,
            max_local_rejects: 65536,
            max_global_rejects: 1024,
            max_flat_map_regens: 1_000_000,
            failure_persistence: FailurePersistence::default(),
            _non_exhaustive: (),
        };

        fn parse_or_warn(dst: &mut u32, value: OsString, var: &str) {
            if let Some(value) = value.to_str() {
                if let Ok(value) = value.parse() {
                    *dst = value;
                } else {
                    eprintln!(
                        "proptest: The env-var {}={} can't be parsed as u32, \
                         using default of {}.", var, value, *dst);
                }
            } else {
                eprintln!(
                    "proptest: The env-var {} is not valid, using \
                     default of {}.", var, *dst);
            }
        }

        for (var, value) in env::vars_os() {
            if let Some(var) = var.to_str() {
                match var {
                    "PROPTEST_CASES" => parse_or_warn(
                        &mut result.cases, value, "PROPTEST_CASES"),
                    "PROPTEST_MAX_LOCAL_REJECTS" => parse_or_warn(
                        &mut result.max_local_rejects, value,
                        "PROPTEST_MAX_LOCAL_REJECTS"),
                    "PROPTEST_MAX_GLOBAL_REJECTS" => parse_or_warn(
                        &mut result.max_global_rejects, value,
                        "PROPTEST_MAX_GLOBAL_REJECTS"),
                    "PROPTEST_MAX_FLAT_MAP_REGENS" => parse_or_warn(
                        &mut result.max_flat_map_regens, value,
                        "PROPTEST_MAX_FLAT_MAP_REGENS"),
                    _ => if var.starts_with("PROPTEST_") {
                        eprintln!("proptest: Ignoring unknown env-var {}.",
                                  var);
                    },
                }
            }
        }

        result
    };
}

/// Configuration for how a proptest test should be run.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The number of successful test cases that must execute for the test as a
    /// whole to pass.
    ///
    /// This does not include implicitly-replayed persisted failing cases.
    ///
    /// The default is 256, which can be overridden by setting the
    /// `PROPTEST_CASES` environment variable.
    pub cases: u32,
    /// The maximum number of individual inputs that may be rejected before the
    /// test as a whole aborts.
    ///
    /// The default is 65536, which can be overridden by setting the
    /// `PROPTEST_MAX_LOCAL_REJECTS` environment variable.
    pub max_local_rejects: u32,
    /// The maximum number of combined inputs that may be rejected before the
    /// test as a whole aborts.
    ///
    /// The default is 1024, which can be overridden by setting the
    /// `PROPTEST_MAX_GLOBAL_REJECTS` environment variable.
    pub max_global_rejects: u32,
    /// The maximum number of times all `Flatten` combinators will attempt to
    /// regenerate values. This puts a limit on the worst-case exponential
    /// explosion that can happen with nested `Flatten`s.
    ///
    /// The default is 1_000_000, which can be overridden by setting the
    /// `PROPTEST_MAX_FLAT_MAP_REGENS` environment variable.
    pub max_flat_map_regens: u32,
    /// Indicates how to determine the file to use for persisting failed test
    /// results.
    ///
    /// See the docs of [`FailurePersistence`](enum.FailurePersistence.html)
    /// for more information.
    ///
    /// The default is `FailurePersistence::WithMain("proptest-failures.txt")`.
    /// The default cannot currently be overridden by an environment variable.
    pub failure_persistence: FailurePersistence,
    // Needs to be public so FRU syntax can be used.
    #[doc(hidden)]
    pub _non_exhaustive: (),
}

impl Config {
    /// Constructs a `Config` only differing from the `default()` in the
    /// number of test cases required to pass the test successfully.
    ///
    /// This is simply a more concise alternative to using field-record update
    /// syntax:
    ///
    /// ```
    /// # use proptest::test_runner::Config;
    /// assert_eq!(
    ///     Config::with_cases(42),
    ///     Config { cases: 42, .. Config::default() }
    /// );
    /// ```
    pub fn with_cases(n: u32) -> Self {
        Self {
            cases: n,
            .. Config::default()
        }
    }
}


impl Default for Config {
    fn default() -> Self {
        DEFAULT_CONFIG.clone()
    }
}

/// Describes how failing test cases are persisted.
///
/// Note that file names in this enum are `&str` rather than `&Path` since
/// constant functions are not yet in Rust stable as of 2017-12-16.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FailurePersistence {
    /// Completely disables persistence of failing test cases.
    ///
    /// This is semantically equivalent to `Direct("/dev/null")` on Unix and
    /// `Direct("NUL")` on Windows (though it is internally handled by simply
    /// not doing any I/O).
    Off,
    /// The path given to `TestRunner::set_source_file()` is parsed, and
    /// proptest traverses the directory tree upwards. The string given in this
    /// value is resolved against the first directory which contains `lib.rs`
    /// or `main.rs`, as per
    /// [`PathBuf::push`](https://doc.rust-lang.org/stable/std/path/struct.PathBuf.html#method.push).
    ///
    /// If no `lib.rs` or `main.rs` can be found, a warning is printed and this
    /// option behaves like `WithSource`. If `TestRunner::set_source_file()`
    /// has not been called, a warning is printed and this option behaves like
    /// `Direct`.
    WithMain(&'static str),
    /// The path given to `TestRunner::set_source_file()` is parsed. The string
    /// given in this value is resolved against the parent directory of the
    /// source file, as per
    /// [`PathBuf::push`](https://doc.rust-lang.org/stable/std/path/struct.PathBuf.html#method.push).
    ///
    /// If `TestRunner::set_source_ifle()` has not been called, a warning is
    /// printed and this option behaves like `Direct`.
    WithSource(&'static str),
    /// The string given in this option is directly used as a file path without
    /// any further processing.
    Direct(&'static str),
    #[doc(hidden)]
    #[allow(missing_docs)]
    _NonExhaustive,
}

impl Default for FailurePersistence {
    fn default() -> Self {
        FailurePersistence::WithMain("proptest-failures.txt")
    }
}

impl FailurePersistence {
    /// Given the nominal source path, determine the location of the failure
    /// persistence file, if any.
    fn resolve(&self, source: Option<&Path>) -> Option<PathBuf> {
        match *self {
            FailurePersistence::Off => None,

            FailurePersistence::WithMain(path) => match source {
                Some(source_path) => {
                    let mut dir = source_path.to_owned();
                    let mut found = false;
                    while dir.pop() {
                        if dir.join("lib.rs").is_file() ||
                            dir.join("main.rs").is_file()
                        {
                            found = true;
                            break;
                        }
                    }

                    if !found {
                        eprintln!("proptest: FailurePersistence::WithMain set, \
                                   but failed to find lib.rs or main.rs");
                        FailurePersistence::WithSource(path).resolve(source)
                    } else {
                        Some(dir.join(path))
                    }
                },
                None => {
                    eprintln!("proptest: FailurePersistence::WithMain set, \
                               but no source file known");
                    Some(Path::new(path).to_owned())
                },
            },

            FailurePersistence::WithSource(path) => match source {
                Some(source_path) =>
                    Some(source_path.parent().unwrap_or(source_path)
                         .join(path)),

                None => {
                    eprintln!("proptest: FailurePersistence::WithSource set, \
                               but no source file known");
                    Some(Path::new(path).to_owned())
                },
            },

            FailurePersistence::Direct(path) =>
                Some(Path::new(path).to_owned()),

            FailurePersistence::_NonExhaustive =>
                panic!("FailurePersistence set to _NonExhaustive"),
        }
    }
}

/// Errors which can be returned from test cases to indicate non-successful
/// completion.
///
/// Note that in spite of the name, `TestCaseError` is currently *not* an
/// instance of `Error`, since otherwise `impl<E : Error> From<E>` could not be
/// provided.
///
/// Any `Error` can be converted to a `TestCaseError`, which places
/// `Error::display()` into the `Fail` case.
#[derive(Debug, Clone)]
pub enum TestCaseError {
    /// The input was not valid for the test case. This does not count as a
    /// test failure (nor a success); rather, it simply signals to generate
    /// a new input and try again.
    ///
    /// The string gives the location and context of the rejection, and
    /// should be suitable for formatting like `Foo did X at {whence}`.
    Reject(String),
    /// The code under test failed the test.
    ///
    /// The string should indicate the location of the failure, but may
    /// generally be any string.
    Fail(String),
}

/// Convenience for the type returned by test cases.
pub type TestCaseResult = Result<(), TestCaseError>;

impl fmt::Display for TestCaseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            TestCaseError::Reject(ref whence) =>
                write!(f, "Input rejected at {}", whence),
            TestCaseError::Fail(ref why) =>
                write!(f, "Case failed: {}", why),
        }
    }
}

impl<E : ::std::error::Error> From<E> for TestCaseError {
    fn from(cause: E) -> Self {
        TestCaseError::Fail(cause.to_string())
    }
}

/// A failure state from running test cases for a single test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestError<T> {
    /// The test was aborted for the given reason, for example, due to too many
    /// inputs having been rejected.
    Abort(String),
    /// A failing test case was found. The string indicates where and/or why
    /// the test failed. The `T` is the minimal input found to reproduce the
    /// failure.
    Fail(String, T),
}

impl<T : fmt::Debug> fmt::Display for TestError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            TestError::Abort(ref why) =>
                write!(f, "Test aborted: {}", why),
            TestError::Fail(ref why, ref what) =>
                write!(f, "Test failed: {}; minimal failing input: {:?}",
                       why, what),
        }
    }
}

impl<T : fmt::Debug> ::std::error::Error for TestError<T> {
    fn description(&self) -> &str {
        match *self {
            TestError::Abort(..) => "Abort",
            TestError::Fail(..) => "Fail",
        }
    }
}

/// State used when running a proptest test.
#[derive(Clone)]
pub struct TestRunner {
    config: Config,
    successes: u32,
    local_rejects: u32,
    global_rejects: u32,
    rng: XorShiftRng,
    flat_map_regens: Arc<AtomicUsize>,

    local_reject_detail: BTreeMap<String, u32>,
    global_reject_detail: BTreeMap<String, u32>,

    source_file: Option<&'static Path>,
}

impl fmt::Debug for TestRunner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TestRunner")
            .field("config", &self.config)
            .field("successes", &self.successes)
            .field("local_rejects", &self.local_rejects)
            .field("global_rejects", &self.global_rejects)
            .field("rng", &"<XorShiftRng>")
            .field("flat_map_regens", &self.flat_map_regens)
            .field("local_reject_detail", &self.local_reject_detail)
            .field("global_reject_detail", &self.global_reject_detail)
            .field("source_file", &self.source_file)
            .finish()
    }
}

impl fmt::Display for TestRunner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "\tsuccesses: {}\n\
                   \tlocal rejects: {}\n",
               self.successes, self.local_rejects)?;
        for (whence, count) in &self.local_reject_detail {
            writeln!(f, "\t\t{} times at {}", count, whence)?;
        }
        writeln!(f, "\tglobal rejects: {}", self.global_rejects)?;
        for (whence, count) in &self.global_reject_detail {
            writeln!(f, "\t\t{} times at {}", count, whence)?;
        }

        Ok(())
    }
}

/// Equivalent to: `TestRunner::new(Config::default())`.
impl Default for TestRunner {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

lazy_static! {
    /// Used to guard access to the persistence file(s) so that a single
    /// process will not step on its own toes.
    ///
    /// We don't have much protecting us should two separate process try to
    /// write to the same file at once (depending on how atomic append mode is
    /// on the OS), but this should be extremely rare.
    static ref PERSISTENCE_LOCK: RwLock<()> = RwLock::new(());
}

fn load_persisted_failures(path: Option<&PathBuf>) -> Vec<[u32;4]> {
    let result: io::Result<Vec<[u32;4]>> =
        path.map_or_else(|| Ok(vec![]), |path| {
            // .ok() instead of .unwrap() so we don't propagate panics here
            let _lock = PERSISTENCE_LOCK.read().ok();

            let mut ret = Vec::new();

            let input = io::BufReader::new(fs::File::open(path)?);
            for (lineno, line) in input.lines().enumerate() {
                let mut line = line?;
                if let Some(comment_start) = line.find('#') {
                    line.truncate(comment_start);
                }

                let parts = line.trim().split(' ').collect::<Vec<_>>();
                if 5 == parts.len() && "xs" == parts[0] {
                    let seed = parts[1].parse::<u32>().and_then(
                        |a| parts[2].parse::<u32>().and_then(
                            |b| parts[3].parse::<u32>().and_then(
                                |c| parts[4].parse::<u32>().map(
                                    |d| [a, b, c, d]))));
                    if let Ok(seed) = seed {
                        ret.push(seed);
                    } else {
                        eprintln!(
                            "proptest: {}:{}: unparsable line, \
                             ignoring", path.display(), lineno + 1);
                    }
                } else if parts.len() > 1 {
                    eprintln!("proptest: {}:{}: unknown case type `{}` \
                               (corrupt file or newer proptest version?)",
                              path.display(), lineno + 1, parts[0]);
                }
            }

            Ok(ret)
        });

    match result {
        Ok(r) => r,
        Err(err) => {
            if io::ErrorKind::NotFound != err.kind() {
                eprintln!(
                    "proptest: failed to open {}: {}",
                    path.map(|x| &**x).unwrap_or(Path::new("??")).display(),
                    err);
            }
            vec![]
        },
    }
}

fn save_persisted_failure(path: Option<&PathBuf>,
                          seed: [u32;4],
                          value: &fmt::Debug) {
    if let Some(path) = path {
        // .ok() instead of .unwrap() so we don't propagate panics here
        let _lock = PERSISTENCE_LOCK.write().ok();
        let is_new = !path.is_file();

        let mut to_write = Vec::<u8>::new();
        if is_new {
            writeln!(to_write, "\
# Seeds for failure cases proptest has generated in the past. It is
# automatically read and these particular cases re-run before any
# novel cases are generated.
#
# It is recommended to check this file in to source control so that
# everyone who runs the test benefits from these saved cases.")
                    .expect("writeln! to vec failed");
            }
            let mut data_line = Vec::<u8>::new();
            write!(data_line, "xs {} {} {} {} # shrinks to {:?}",
                   seed[0], seed[1], seed[2], seed[3],
                   value).expect("write! to vec failed");
            // Ensure there are no newlines in the debug output
            for byte in &mut data_line {
                if b'\n' == *byte || b'\r' == *byte {
                    *byte = b' ';
                }
            }

            to_write.extend(data_line);
            to_write.push(b'\n');

            let mut options = fs::OpenOptions::new();
            options.append(true).create(true);
            let res = options.open(path).and_then(
                |mut out| out.write_all(&to_write));

            if let Err(e) = res {
                eprintln!(
                    "proptest: failed to append to {}: {}",
                    path.display(), e);
            } else if is_new {
                eprintln!(
                    "proptest: Saving this and future failures in {}",
                    path.display());
            }
        }
    }

impl TestRunner {
    /// Create a fresh `TestRunner` with the given configuration.
    pub fn new(config: Config) -> Self {
        TestRunner {
            config: config,
            successes: 0,
            local_rejects: 0,
            global_rejects: 0,
            rng: rand::weak_rng(),
            flat_map_regens: Arc::new(AtomicUsize::new(0)),
            local_reject_detail: BTreeMap::new(),
            global_reject_detail: BTreeMap::new(),
            source_file: None,
        }
    }

    /// Create a fresh `TestRunner` with the same config and global counters as
    /// this one, but with local state reset and an independent `Rng`.
    pub(crate) fn partial_clone(&self) -> Self {
        TestRunner {
            config: self.config.clone(),
            successes: 0,
            local_rejects: 0,
            global_rejects: 0,
            rng: rand::weak_rng(),
            flat_map_regens: Arc::clone(&self.flat_map_regens),
            local_reject_detail: BTreeMap::new(),
            global_reject_detail: BTreeMap::new(),
            source_file: self.source_file,
        }
    }

    /// Returns the RNG for this test run.
    pub fn rng(&mut self) -> &mut XorShiftRng {
        &mut self.rng
    }

    /// Returns the configuration of this runner.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Set the source file to use for resolving the location of the persisted
    /// failing cases file.
    ///
    /// See [`FailurePersistence`](enum.FailurePersistence.html) for details on
    /// how this value is used.
    ///
    /// This is normally called automatically by the `proptest!` macro, which
    /// passes `file!()`.
    pub fn set_source_file(&mut self, source: &'static Path) {
        self.source_file = Some(source);
    }

    /// Run test cases against `f`, choosing inputs via `strategy`.
    ///
    /// If any failure cases occur, try to find a minimal failure case and
    /// report that. If invoking `f` panics, the panic is turned into a
    /// `TestCaseError::Fail`.
    ///
    /// If failure persistence is enabled, all persisted failing cases are
    /// tested first. If a later non-persisted case fails, its seed is
    /// persisted before returning failure.
    ///
    /// Returns success or failure indicating why the test as a whole failed.
    pub fn run<S : Strategy,
               F : Fn (&ValueFor<S>) -> TestCaseResult>
        (&mut self, strategy: &S, f: F)
         -> Result<(), TestError<ValueFor<S>>>
    {
        let persist_path =
            self.config.failure_persistence.resolve(self.source_file);

        let old_rng = self.rng.clone();
        for persisted_seed in load_persisted_failures(persist_path.as_ref())
        {
            self.rng = XorShiftRng::from_seed(persisted_seed);
            self.gen_and_run_case(strategy, &f)?;
        }
        self.rng = old_rng;

        while self.successes < self.config.cases {
            // Generate a new seed and make an RNG from that so that we know
            // what seed to persist if this case fails.
            let seed = <[u32;4] as Rand>::rand(&mut self.rng);
            self.rng = XorShiftRng::from_seed(seed);
            let result = self.gen_and_run_case(strategy, &f);
            if let Err(TestError::Fail(_, ref value)) = result {
                save_persisted_failure(persist_path.as_ref(), seed, value);
            }

            let _ = result?;
        }

        Ok(())
    }

    fn gen_and_run_case<S : Strategy, F : Fn (&ValueFor<S>) -> TestCaseResult>
        (&mut self, strategy: &S, f: &F)
        -> Result<(), TestError<ValueFor<S>>>
    {
        let case = match strategy.new_value(self) {
            Ok(v) => v,
            Err(msg) => return Err(TestError::Abort(msg)),
        };
        if self.run_one(case, f)? {
            self.successes += 1;
        }
        Ok(())
    }

    /// Run one specific test case against this runner.
    ///
    /// If the test fails, finds the minimal failing test case. If the test
    /// does not fail, returns whether it succeeded or was filtered out.
    pub fn run_one<V : ValueTree,
                   F : Fn (&V::Value) -> TestCaseResult>
        (&mut self, mut case: V, f: F) -> Result<bool, TestError<V::Value>>
    {
        macro_rules! test {
            ($v:expr) => { {
                let v = $v;
                match panic::catch_unwind(AssertUnwindSafe(|| f(&v))) {
                    Ok(r) => r,
                    Err(what) => {
                        let msg = what.downcast::<&'static str>()
                            .map(|s| (*s).to_owned())
                            .or_else(|what| what.downcast::<String>().map(|b| *b))
                            .unwrap_or_else(
                                |_| "<unknown panic value>".to_owned());
                        Err(TestCaseError::Fail(msg))
                    },
                }
            } }
        }

        match test!(case.current()) {
            Ok(_) => Ok(true),
            Err(TestCaseError::Fail(why)) => {
                let mut last_failure = (why, case.current());
                if case.simplify() {
                    loop {
                        let passed = match test!(case.current()) {
                            // Rejections are effectively a pass here,
                            // since they indicate that any behaviour of
                            // the function under test is acceptable.
                            Ok(_) | Err(TestCaseError::Reject(..)) => true,

                            Err(TestCaseError::Fail(why)) => {
                                last_failure = (why, case.current());
                                false
                            },
                        };

                        if passed {
                            if !case.complicate() {
                                break;
                            }
                        } else if !case.simplify() {
                            break;
                        }
                    }
                }

                Err(TestError::Fail(last_failure.0, last_failure.1))
            },
            Err(TestCaseError::Reject(whence)) => {
                self.reject_global(&whence)?;
                Ok(false)
            },
        }
    }

    /// Update the state to account for a local rejection from `whence`, and
    /// return `Ok` if the caller should keep going or `Err` to abort.
    pub fn reject_local(&mut self, whence: String) -> Result<(),String> {
        if self.local_rejects >= self.config.max_local_rejects {
            Err("Too many local rejects".to_owned())
        } else {
            self.local_rejects += 1;
            let need_insert = if let Some(count) =
                self.local_reject_detail.get_mut(&whence)
            {
                *count += 1;
                false
            } else {
                true
            };
            if need_insert {
                self.local_reject_detail.insert(whence, 1);
            }

            Ok(())
        }
    }

    /// Update the state to account for a global rejection from `whence`, and
    /// return `Ok` if the caller should keep going or `Err` to abort.
    fn reject_global<T>(&mut self, whence: &str) -> Result<(),TestError<T>> {
        if self.global_rejects >= self.config.max_global_rejects {
            Err(TestError::Abort("Too many global rejects".to_owned()))
        } else {
            self.global_rejects += 1;
            let need_insert = if let Some(count) =
                self.global_reject_detail.get_mut(whence)
            {
                *count += 1;
                false
            } else {
                true
            };
            if need_insert {
                self.global_reject_detail.insert(whence.to_owned(), 1);
            }

            Ok(())
        }
    }

    /// Increment the counter of flat map regenerations and return whether it
    /// is still under the configured limit.
    pub fn flat_map_regen(&self) -> bool {
        self.flat_map_regens.fetch_add(1, SeqCst) <
            self.config.max_flat_map_regens as usize
    }
}

#[cfg(test)]
mod test {
    use std::cell::Cell;
    use std::fs;

    use super::*;
    use strategy::Strategy;


    #[test]
    fn gives_up_after_too_many_rejections() {
        let config = Config::default();
        let mut runner = TestRunner::new(config.clone());
        let runs = Cell::new(0);
        let result = runner.run(&(0u32..), |_| {
            runs.set(runs.get() + 1);
            Err(TestCaseError::Reject("reject".to_owned()))
        });
        match result {
            Err(TestError::Abort(_)) => (),
            e => panic!("Unexpected result: {:?}", e),
        }
        assert_eq!(config.max_global_rejects + 1, runs.get());
    }

    #[test]
    fn test_pass() {
        let mut runner = TestRunner::default();
        let result = runner.run(&(1u32..), |&v| { assert!(v > 0); Ok(()) });
        assert_eq!(Ok(()), result);
    }

    #[test]
    fn test_fail_via_result() {
        let mut runner = TestRunner::new(Config {
            failure_persistence: FailurePersistence::Off,
            .. Config::default()
        });
        let result = runner.run(&(0u32..10u32), |&v| if v < 5 {
            Ok(())
        } else {
            Err(TestCaseError::Fail("not less than 5".to_owned()))
        });

        assert_eq!(Err(TestError::Fail("not less than 5".to_owned(), 5)),
                   result);
    }

    #[test]
    fn test_fail_via_panic() {
        let mut runner = TestRunner::new(Config {
            failure_persistence: FailurePersistence::Off,
            .. Config::default()
        });
        let result = runner.run(&(0u32..10u32), |&v| {
            assert!(v < 5, "not less than 5");
            Ok(())
        });
        assert_eq!(Err(TestError::Fail("not less than 5".to_owned(), 5)),
                   result);
    }

    struct TestPaths {
        crate_root: &'static Path,
        lib_root: PathBuf,
        src_subdir: PathBuf,
        src_file: PathBuf,
        subdir_file: PathBuf,
        misplaced_file: PathBuf,
    }

    lazy_static! {
        static ref TEST_PATHS: TestPaths = {
            let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
            let lib_root = crate_root.join("src");
            let src_subdir = lib_root.join("strategy");
            let src_file = lib_root.join("foo.rs");
            let subdir_file = src_subdir.join("foo.rs");
            let misplaced_file = crate_root.join("foo.rs");
            TestPaths {
                crate_root, lib_root, src_subdir,
                src_file, subdir_file, misplaced_file
            }
        };
    }

    // This test assumes UNIX-like paths
    #[cfg(unix)]
    #[test]
    fn persistence_file_location_resolved_correctly() {
        // If off, there is never a file
        assert_eq!(None, FailurePersistence::Off.resolve(None));
        assert_eq!(None, FailurePersistence::Off.resolve(
            Some(&TEST_PATHS.subdir_file)));

        // For direct, we don't care about the source file, and instead always
        // use whatever is in the config.
        assert_eq!(Some(Path::new("bar.txt").to_owned()),
                   FailurePersistence::Direct("bar.txt").resolve(None));
        assert_eq!(Some(Path::new("bar.txt").to_owned()),
                   FailurePersistence::Direct("bar.txt").resolve(
                       Some(&TEST_PATHS.subdir_file)));

        // For WithSource, it should be located under src_subdir if the source
        // file is available and the configuration is relative. Otherwise, it
        // is the configuration exactly.
        assert_eq!(Some(TEST_PATHS.src_subdir.join("bar.txt")),
                   FailurePersistence::WithSource("bar.txt").resolve(
                       Some(&TEST_PATHS.subdir_file)));
        assert_eq!(Some(Path::new("/foo/bar.txt").to_owned()),
                   FailurePersistence::WithSource("/foo/bar.txt").resolve(
                       Some(&TEST_PATHS.subdir_file)));
        assert_eq!(Some(Path::new("bar.txt").to_owned()),
                   FailurePersistence::WithSource("bar.txt").resolve(None));

        // For WithMain, a file alongside lib.rs or in a subdirectory results
        // in a file in the same dir as lib.rs...
        assert_eq!(Some(TEST_PATHS.lib_root.join("bar.txt")),
                   FailurePersistence::WithMain("bar.txt").resolve(
                       Some(&TEST_PATHS.subdir_file)));
        assert_eq!(Some(TEST_PATHS.lib_root.join("bar.txt")),
                   FailurePersistence::WithMain("bar.txt").resolve(
                       Some(&TEST_PATHS.src_file)));
        // ... but if neither lib.rs nor main.rs can be found, it ends up
        // alongside the source ...
        assert_eq!(Some(TEST_PATHS.crate_root.join("bar.txt")),
                   FailurePersistence::WithMain("bar.txt").resolve(
                       Some(&TEST_PATHS.misplaced_file)));
        // ... but if the configured path is absolute, the result is also
        // absolute in both cases ...
        assert_eq!(Some(Path::new("/foo/bar.txt").to_owned()),
                   FailurePersistence::WithMain("/foo/bar.txt").resolve(
                       Some(&TEST_PATHS.src_file)));
        assert_eq!(Some(Path::new("/foo/bar.txt").to_owned()),
                   FailurePersistence::WithMain("/foo/bar.txt").resolve(
                       Some(&TEST_PATHS.misplaced_file)));
        // ... and if no source is available, we just use the raw path
        assert_eq!(Some(Path::new("bar.txt").to_owned()),
                   FailurePersistence::WithMain("bar.txt").resolve(None));
    }

    #[derive(Clone, Copy, PartialEq)]
    struct PoorlyBehavedDebug(i32);
    impl fmt::Debug for PoorlyBehavedDebug {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "\r\n{:?}\r\n", self.0)
        }
    }

    #[test]
    fn failing_cases_persisted_and_reloaded() {
        const FILE: &'static str = "persistence-test.txt";
        let _ = fs::remove_file(FILE);

        let max = 10_000_000i32;
        let input = (0i32..max).prop_map(PoorlyBehavedDebug);
        let config = Config {
            failure_persistence: FailurePersistence::Direct(FILE),
            .. Config::default()
        };

        // First test with cases that fail above half max, and then below half
        // max, to ensure we can correctly parse both lines of the persistence
        // file.
        let first_sub_failure = {
            let mut runner = TestRunner::new(config.clone());
            runner.run(&input, |v| {
                if v.0 < max/2 {
                    Ok(())
                } else {
                    Err(TestCaseError::Fail("too big".to_owned()))
                }
            }).err().expect("didn't fail?")
        };
        let first_super_failure = {
            let mut runner = TestRunner::new(config.clone());
            runner.run(&input, |v| {
                if v.0 >= max/2 {
                    Ok(())
                } else {
                    Err(TestCaseError::Fail("too small".to_owned()))
                }
            }).err().expect("didn't fail?")
        };
        let second_sub_failure = {
            let mut runner = TestRunner::new(config.clone());
            runner.run(&input, |v| {
                if v.0 < max/2 {
                    Ok(())
                } else {
                    Err(TestCaseError::Fail("too big".to_owned()))
                }
            }).err().expect("didn't fail?")
        };
        let second_super_failure = {
            let mut runner = TestRunner::new(config.clone());
            runner.run(&input, |v| {
                if v.0 >= max/2 {
                    Ok(())
                } else {
                    Err(TestCaseError::Fail("too small".to_owned()))
                }
            }).err().expect("didn't fail?")
        };

        assert_eq!(first_sub_failure, second_sub_failure);
        assert_eq!(first_super_failure, second_super_failure);
    }
}
