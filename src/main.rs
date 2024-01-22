use clap::Parser;
use rand::seq::SliceRandom;
use rand::Rng;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::{fs, process::Command};

const AFL_CORPUS: &str = "/tmp/afl_input";
const AFL_OUTPUT: &str = "/tmp/afl_output";

// -----------------------------------------
// AFLPlusPlus flags
// Based on: https://aflplus.plus/docs/env_variables/
// -----------------------------------------
#[derive(Debug, Clone)]
pub struct AflConfig {
    pub autoresume: bool,
    pub final_sync: bool,
    pub disable_trim: bool,
    pub keep_timeouts: bool,
    pub expand_havoc_now: bool,
    pub ignore_seed_problems: bool,
    pub import_first: bool,
}

impl Default for AflConfig {
    fn default() -> Self {
        Self {
            // `AFL_AUTORESUME` will resume a fuzz run (same as providing -i -) for an existing out folder, even if a different -i was provided.
            // Without this setting, afl-fuzz will refuse execution for a long-fuzzed out dir
            autoresume: true,

            // `AFL_FINAL_SYNC` will cause the fuzzer to perform a final import of test cases when terminating.
            // This is beneficial for -M main fuzzers to ensure it has all unique test cases and hence you only need to afl-cmin this single queue.
            final_sync: false,

            // Setting `AFL_DISABLE_TRIM` tells afl-fuzz not to trim test cases.
            disable_trim: false,

            //  Setting `AFL_KEEP_TIMEOUTS` will keep longer running inputs if they reach new coverage
            keep_timeouts: false,

            // Setting `AFL_EXPAND_HAVOC_NOW` will start in the extended havoc mode that includes costly mutations.
            // afl-fuzz automatically enables this mode when deemed useful otherwise.
            expand_havoc_now: false,

            // `AFL_IGNORE_SEED_PROBLEMS` will skip over crashes and timeouts in the seeds instead of exiting.
            ignore_seed_problems: false,

            // When setting `AFL_IMPORT_FIRST` tests cases from other fuzzers in the campaign are loaded first.
            // Note: This can slow down the start of the first fuzz
            // by quite a lot of you have many fuzzers and/or many seeds.
            import_first: false,
        }
    }
}

impl AflConfig {
    // Constructor to create a new AflConfig with default values (all flags set to false)
    pub fn new() -> Self {
        Self::default()
    }

    // Setters for each configuration flag
    pub fn set_autorestart(&mut self, value: bool) -> &mut Self {
        self.autoresume = value;
        self
    }

    pub fn set_final_sync(&mut self, value: bool) -> &mut Self {
        self.final_sync = value;
        self
    }

    pub fn set_disable_trim(&mut self, value: bool) -> &mut Self {
        self.disable_trim = value;
        self
    }

    pub fn set_keep_timeouts(&mut self, value: bool) -> &mut Self {
        self.keep_timeouts = value;
        self
    }

    pub fn set_expand_havoc_now(&mut self, value: bool) -> &mut Self {
        self.expand_havoc_now = value;
        self
    }

    pub fn set_ignore_seed_problems(&mut self, value: bool) -> &mut Self {
        self.ignore_seed_problems = value;
        self
    }

    pub fn set_import_first(&mut self, value: bool) -> &mut Self {
        self.import_first = value;
        self
    }

    // Generates the command string based on the current configuration
    pub fn generate_command(&self) -> String {
        let mut command = String::new();

        command.push_str(&format!("AFL_AUTORESUME={} ", u8::from(self.autoresume)));
        command.push_str(&format!("AFL_FINAL_SYNC={} ", u8::from(self.final_sync)));
        command.push_str(&format!(
            "AFL_DISABLE_TRIM={} ",
            u8::from(self.disable_trim)
        ));
        command.push_str(&format!(
            "AFL_KEEP_TIMEOUTS={} ",
            u8::from(self.keep_timeouts)
        ));
        command.push_str(&format!(
            "AFL_EXPAND_HAVOC_NOW={} ",
            u8::from(self.expand_havoc_now)
        ));
        command.push_str(&format!(
            "AFL_IGNORE_SEED_PROBLEMS={} ",
            u8::from(self.ignore_seed_problems)
        ));
        command.push_str(&format!(
            "AFL_IMPORT_FIRST={} ",
            u8::from(self.import_first)
        ));

        command
    }
}

fn apply_flags<F>(configs: &mut [AflConfig], flag_accessor: F, percentage: f64, rng: &mut impl Rng)
where
    F: Fn(&mut AflConfig) -> &mut bool,
{
    let count = (configs.len() as f64 * percentage) as usize;
    let mut indices = HashSet::new();
    while indices.len() < count {
        indices.insert(rng.gen_range(0..configs.len()));
    }

    for index in indices {
        *flag_accessor(&mut configs[index]) = true;
    }
}

fn apply_constrained_args(strings: &mut [String], args: &[(&str, f64)], rng: &mut impl Rng) {
    let n = strings.len();
    for &(arg, percentage) in args {
        let count = (n as f64 * percentage) as usize;
        let mut available_indices: Vec<usize> = (0..n)
            .filter(|i| !strings[*i].contains(arg.split_whitespace().next().unwrap()))
            .collect();
        available_indices.shuffle(rng);

        for &index in available_indices.iter().take(count) {
            strings[index].push_str(&format!(" {arg}"));
        }
    }
}

fn apply_args(strings: &mut [String], arg: &str, percentage: f64, rng: &mut impl Rng) {
    let count = (strings.len() as f64 * percentage) as usize;
    let mut indices = HashSet::new();
    while indices.len() < count {
        indices.insert(rng.gen_range(0..strings.len()));
    }

    for index in indices {
        strings[index].push_str(&format!(" {arg}"));
    }
}

struct Harness {
    // Instrumented and maybe AFL_HARDEN=1
    target_binary: PathBuf,
    // AFL_USE_*SAN=1
    sanitizer_binary: Option<PathBuf>,
    // AFL_LLVM_CMPLOG=1
    cmplog_binary: Option<PathBuf>,
    // Additional arguments for the harness
    // If the harness reads from stdin, use @@ as placeholder
    target_args: Option<String>,
}

impl Harness {
    pub fn new<P: Into<PathBuf> + std::convert::AsRef<std::ffi::OsStr> + std::fmt::Display>(
        target_binary: P,
        sanitizer_binary: Option<P>,
        cmplog_binary: Option<P>,
        target_args: Option<String>,
    ) -> Self {
        let target_binary = Self::_get_target_binary(target_binary);
        assert!(target_binary.is_some(), "Could not find target binary");

        let sanitizer_binary = Self::_get_sanitizer_binary(sanitizer_binary);
        let cmplog_binary = Self::_get_cmplog_binary(cmplog_binary);

        Self {
            target_binary: target_binary.unwrap(),
            sanitizer_binary,
            cmplog_binary,
            target_args: target_args.map(std::convert::Into::into),
        }
    }

    fn _is_path_binary<P: Into<PathBuf> + std::convert::AsRef<std::ffi::OsStr>>(path: &P) -> bool {
        let path: PathBuf = path.into();
        path.exists() && path.is_file()
    }

    fn _get_target_binary<P: Into<PathBuf> + std::convert::AsRef<std::ffi::OsStr>>(
        target_binary: P,
    ) -> Option<PathBuf> {
        let target_binary = target_binary.into();
        if Self::_is_path_binary(&target_binary) {
            let resolved_tbin = fs::canonicalize(target_binary).expect("Failed to resolve path");
            return Some(resolved_tbin);
        }
        None
    }

    fn _get_sanitizer_binary<P: Into<PathBuf> + std::convert::AsRef<std::ffi::OsStr>>(
        sanitizer_binary: Option<P>,
    ) -> Option<PathBuf> {
        let sanitizer_binary = sanitizer_binary.map_or_else(PathBuf::new, std::convert::Into::into);
        if Self::_is_path_binary(&sanitizer_binary) {
            let res_sbin = fs::canonicalize(sanitizer_binary).expect("Failed to resolve path");
            return Some(res_sbin);
        }
        None
    }

    fn _get_cmplog_binary<P: Into<PathBuf> + std::convert::AsRef<std::ffi::OsStr>>(
        cmplog_binary: Option<P>,
    ) -> Option<PathBuf> {
        let cmplog_binary = cmplog_binary.map_or_else(PathBuf::new, std::convert::Into::into);
        if Self::_is_path_binary(&cmplog_binary) {
            let cmpl_bin = fs::canonicalize(cmplog_binary).expect("Failed to resolve path");
            return Some(cmpl_bin);
        }
        None
    }
}

struct AFLRunner {
    // Path to afl-fuzz
    afl_binary: PathBuf,
    // harnesses
    harness: Harness,
    // AFL env flags setting
    // Corpus directory
    input_dir: PathBuf,
    // Output directory
    output_dir: PathBuf,
    // Amount of runners
    runners: u32,
    // Dictionary
    dictionary: Option<String>,
}

impl Default for AFLRunner {
    fn default() -> Self {
        Self {
            afl_binary: PathBuf::new(),
            harness: Harness::new(String::new(), None, None, None),
            input_dir: PathBuf::new(),
            output_dir: PathBuf::new(),
            runners: 1,
            dictionary: None,
        }
    }
}

impl AFLRunner {
    pub fn new<P: Into<PathBuf> + AsRef<OsStr> + Copy + Display>(
        harness: Harness,
        runners: u32,
        afl_binary: Option<P>,
        input_dir: P,
        output_dir: P,
        dictionary: Option<P>,
    ) -> Self {
        let afl_binary = Self::_get_afl_fuzz(afl_binary).expect("Could not find afl-fuzz binary");

        Self::_mkdir_helper(&input_dir, false);
        fs::write(format!("{}/1", input_dir), "fuzz").expect("Failed to write to file");

        Self::_mkdir_helper(&output_dir, true);

        let dict = dictionary.and_then(|d| {
            let dict_path = d.into();
            if dict_path.exists() && dict_path.is_file() {
                dict_path.to_str().map(String::from)
            } else {
                None
            }
        });

        Self {
            afl_binary,
            harness,
            input_dir: input_dir.into(),
            output_dir: output_dir.into(),
            runners,
            dictionary: dict,
        }
    }

    fn _mkdir_helper<P: Into<PathBuf> + AsRef<OsStr> + Copy>(dir: &P, check_empty: bool) {
        let dir: PathBuf = dir.into();
        assert!(!dir.is_file(), "{} is a file", dir.display());
        if check_empty {
            let is_empty = dir.read_dir().map_or(true, |mut i| i.next().is_none());
            assert!(is_empty, "{} exists and is not empty", dir.display());
        }
        if !dir.exists() {
            fs::create_dir(&dir).expect("Failed to create directory");
        }
    }
    fn _get_afl_fuzz<P: Into<PathBuf> + AsRef<OsStr>>(afl_fuzz: Option<P>) -> Option<PathBuf> {
        afl_fuzz
            .map(Into::into)
            .or_else(Self::_get_afl_fuzz_from_path)
            .or_else(Self::_get_afl_fuzz_from_env)
            .and_then(|path| {
                if path.exists() && path.is_file() && path.ends_with("afl-fuzz") {
                    Some(path)
                } else {
                    None
                }
            })
    }

    fn _get_afl_fuzz_from_env() -> Option<PathBuf> {
        std::env::var("AFL_PATH").ok().and_then(|path| {
            let afl_path = PathBuf::from(path);
            if afl_path.exists() && afl_path.is_file() && afl_path.ends_with("afl-fuzz") {
                Some(afl_path)
            } else {
                None
            }
        })
    }

    fn _get_afl_fuzz_from_path() -> Option<PathBuf> {
        let output = Command::new("which")
            .arg("afl-fuzz")
            .output()
            .expect("Failed to execute 'which'");
        if output.status.success() {
            let afl_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !afl_path.is_empty() {
                Some(PathBuf::from(afl_path))
            } else {
                None
            }
        } else {
            None
        }
    }
    fn generate_afl_commands(&self) -> Vec<String> {
        let mut rng = rand::thread_rng();
        let configs = self.initialize_configs(&mut rng);
        let mut strings = self.create_initial_strings(&configs);

        self.apply_mutation_strategies(&mut strings, &mut rng);
        self.apply_queue_selection(&mut strings, &mut rng);
        self.apply_power_schedules(&mut strings);
        self.set_directories(&mut strings);
        self.set_fuzzer_roles(&mut strings);
        self.apply_dictionary(&mut strings);
        self.set_sanitizer_or_target_binary(&mut strings);
        self.configure_cmplog(&mut strings, &mut rng);
        self.apply_target_args(&mut strings);

        strings
    }

    fn initialize_configs(&self, rng: &mut impl Rng) -> Vec<AflConfig> {
        let mut configs = vec![AflConfig::new(); self.runners as usize];
        configs.last_mut().unwrap().final_sync = true;

        apply_flags(&mut configs, |c| &mut c.disable_trim, 0.65, rng);
        apply_flags(&mut configs, |c| &mut c.keep_timeouts, 0.5, rng);
        apply_flags(&mut configs, |c| &mut c.expand_havoc_now, 0.4, rng);
        configs
    }

    fn create_initial_strings(&self, configs: &[AflConfig]) -> Vec<String> {
        configs
            .iter()
            .map(|config| {
                format!(
                    "{} {}",
                    config.generate_command(),
                    self.afl_binary.display()
                )
            })
            .collect()
    }

    fn apply_mutation_strategies(&self, strings: &mut [String], rng: &mut impl Rng) {
        // Apply different mutation strategies to a percentage of the configs
        let mode_args = [("-P explore", 0.4), ("-P exploit", 0.2)];
        apply_constrained_args(strings, &mode_args, rng);
        let format_args = [("-a binary", 0.3), ("-a text", 0.3)];
        apply_constrained_args(strings, &format_args, rng);
        apply_args(strings, "-L 0", 0.1, rng);
    }

    fn apply_queue_selection(&self, strings: &mut [String], rng: &mut impl Rng) {
        // Applt sequential queue selection to 20% of the configs
        apply_args(strings, "-Z", 0.2, rng);
    }

    fn apply_power_schedules(&self, strings: &mut [String]) {
        // Cycle through the different power schedules for the available runners
        let pscheds = ["fast", "explore", "coe", "lin", "quad", "exploit", "rare"];
        strings.iter_mut().enumerate().for_each(|(i, s)| {
            s.push_str(&format!(" -p {}", pscheds[i % pscheds.len()]));
        });
    }

    fn set_directories(&self, strings: &mut Vec<String>) {
        for s in strings {
            s.push_str(&format!(
                " -i {} -o {}",
                self.input_dir.display(),
                self.output_dir.display()
            ));
        }
    }

    fn set_fuzzer_roles(&self, strings: &mut [String]) {
        let target_fname = self
            .harness
            .target_binary
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        // Set one main fuzzer
        strings[0].push_str(&format!(" -M main_{target_fname}"));
        // Set the rest to be slaves
        for (i, s) in strings[1..].iter_mut().enumerate() {
            s.push_str(&format!(" -S slave_{i}_{target_fname}"));
        }
    }

    fn apply_dictionary(&self, strings: &mut Vec<String>) {
        // If a dictionary is provided, set it for all configs
        if let Some(dict) = self.dictionary.as_ref() {
            let dict_path = fs::canonicalize(dict).expect("Failed to resolve dictionary path");
            for s in strings {
                s.push_str(&format!(" -x {}", dict_path.display()));
            }
        }
    }

    fn set_sanitizer_or_target_binary(&self, strings: &mut [String]) {
        // Set the first one to be a sanitizer_binary if available, otherwise the target_binary
        let binary = self
            .harness
            .sanitizer_binary
            .as_ref()
            .unwrap_or(&self.harness.target_binary);
        strings[0].push_str(&format!(" -- {}", binary.display()));
    }

    fn configure_cmplog(&self, strings: &mut [String], rng: &mut impl Rng) {
        if let Some(cmplog_binary) = self.harness.cmplog_binary.as_ref() {
            let num_cmplog_cfgs = (f64::from(self.runners) * 0.3) as usize;
            match num_cmplog_cfgs {
                0 => {
                    // We have a cmplog binary but not enough config slots to use it
                }
                1 => {
                    // We have exactly one runner available for cmplog so we use `-l 2`
                    strings[1].push_str(
                        format!(
                            " -l 2 -c {} -- {}",
                            cmplog_binary.display(),
                            self.harness.target_binary.display()
                        )
                        .as_str(),
                    );
                }
                2 => {
                    // We have exactly two runners available for cmplog so we use `-l 2` and `-l 2AT`
                    strings[1].push_str(
                        format!(
                            " -l 2 -c {} -- {}",
                            cmplog_binary.display(),
                            self.harness.target_binary.display()
                        )
                        .as_str(),
                    );
                    strings[2].push_str(
                        format!(
                            " -l 2AT -c {} -- {}",
                            cmplog_binary.display(),
                            self.harness.target_binary.display()
                        )
                        .as_str(),
                    );
                }
                3 => {
                    // We can now use all three modes
                    strings[1].push_str(
                        format!(
                            " -l 2 -c {} -- {}",
                            cmplog_binary.display(),
                            self.harness.target_binary.display()
                        )
                        .as_str(),
                    );
                    strings[2].push_str(
                        format!(
                            " -l 2AT -c {} -- {}",
                            cmplog_binary.display(),
                            self.harness.target_binary.display()
                        )
                        .as_str(),
                    );
                    strings[3].push_str(
                        format!(
                            " -l 3 -c {} -- {}",
                            cmplog_binary.display(),
                            self.harness.target_binary.display()
                        )
                        .as_str(),
                    );
                }
                _ => {
                    // We have more than 3 runners available for cmplog so we use all three modes with
                    // the following distribution:
                    // - 70% for -l 2
                    // - 10% for -l 3
                    // - 20% for -l 2AT.
                    self.apply_cmplog_instrumentation(strings, num_cmplog_cfgs, cmplog_binary, rng);
                    self.apply_normal_instrumentation(strings, num_cmplog_cfgs);
                }
            }
        } else {
            self.apply_normal_instrumentation(strings, 0);
        }
    }

    fn apply_cmplog_instrumentation(
        &self,
        strings: &mut [String],
        num_cmplog_cfgs: usize,
        cmplog_binary: &Path,
        rng: &mut impl Rng,
    ) {
        let cmplog_args = [("-l 2 ", 0.7), ("-l 3", 0.1), ("-l 2AT", 0.2)];
        apply_constrained_args(&mut strings[1..=num_cmplog_cfgs], &cmplog_args, rng);
        for s in &mut strings[1..=num_cmplog_cfgs] {
            s.push_str(
                format!(
                    " -c {} -- {}",
                    cmplog_binary.display(),
                    self.harness.target_binary.display()
                )
                .as_str(),
            );
        }
    }

    fn apply_normal_instrumentation(&self, strings: &mut [String], start_index: usize) {
        for s in &mut strings[start_index + 1..] {
            s.push_str(format!(" -- {}", self.harness.target_binary.display()).as_str());
        }
    }

    fn apply_target_args(&self, strings: &mut [String]) {
        // Appends target arguments to the command string
        if let Some(target_args) = self.harness.target_args.as_ref() {
            for s in strings {
                s.push_str(format!(" {target_args}").as_str());
            }
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "Parallelized AFLPlusPlus Campaign Runner")]
#[command(author = "C.K. <admin@0x434b.dev>")]
#[command(version = "1.0")]
pub struct Args {
    /// Target binary to fuzz
    #[arg(short, long, help = "Instrumented target binary to fuzz")]
    target: String,
    /// Sanitizer binary to use
    #[arg(
        short,
        long,
        help = "Instrumented with *SAN binary to use",
        required = false
    )]
    san_target: Option<String>,
    /// CMPLOG binary to use
    #[arg(
        short,
        long,
        help = "Instrumented with CMPLOG binary to use",
        required = false
    )]
    cmpl_target: Option<String>,
    /// Target binary arguments
    #[arg(
        short = 'a',
        long,
        help = "Target binary arguments, including @@ if needed",
        required = false
    )]
    target_args: Option<String>,
    /// Amount of processes to spin up
    #[arg(
        short,
        long,
        default_value = "1",
        value_name = "NUM_PROCS",
        help = "Amount of processes to spin up"
    )]
    runners: u32,
    /// Corpus directory
    #[arg(
        short,
        long,
        default_value = AFL_CORPUS,
        help = "Corpus directory",
        required = false
    )]
    input_dir: Option<String>,
    /// Output directory
    #[arg(
        short,
        long,
        default_value = AFL_OUTPUT,
        help = "Output directory",
        required = false
    )]
    output_dir: Option<String>,
    /// Path to dictionary
    #[arg(
        short = 'x',
        long,
        default_value = None,
        value_name = "DICT_FILE",
        help = "Dictionary to use",
        required = false
    )]
    dictionary: Option<String>,
    /// AFL-Fuzz binary
    #[arg(
        short = 'b',
        long,
        default_value = None,
        help = "Custom path to 'afl-fuzz' binary. If not specified and 'afl-fuzz' is not in $PATH, the program will try to use $AFL_PATH",
        required = false
    )]
    afl_binary: Option<String>,
    #[arg(
        long,
        help = "Spin up a custom tmux session with the fuzzers",
        required = false
    )]
    use_tmux: bool,
}

fn main() {
    let cli_args = Args::parse();
    let harness = Harness::new(
        cli_args.target,
        cli_args.san_target,
        cli_args.cmpl_target,
        cli_args.target_args,
    );
    let afl_runner = AFLRunner::new(
        harness,
        cli_args.runners,
        cli_args.afl_binary.as_ref(),
        &cli_args.input_dir.unwrap(),
        &cli_args.output_dir.unwrap(),
        cli_args.dictionary.as_ref(),
    );
    let cmds = afl_runner.generate_afl_commands();

    if cli_args.use_tmux {
        println!("TODO");
    } else {
        for cmd in cmds {
            println!("{cmd}");
        }
    }
}
