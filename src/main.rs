use clap::Parser;
use reclaim::cli::{Args, Format};
use reclaim::report::{JsonReporter, Reporter, TableReporter, TreeReporter};
use reclaim::scanner::ScanOptions;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = Args::parse();

    if args.tui {
        return match reclaim::tui::run_tui(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let threads = args.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    let options = ScanOptions {
        threads,
        min_size: args.min_size,
        include_hidden: args.hidden,
    };

    let mut result = reclaim::scanner::scan(&args.path, &options);

    // 重复检测的 warnings 并入结果，交给 reporter 统一渲染。
    let dups = if args.dup {
        let (groups, warnings) = reclaim::dedup::find_duplicates(&result.entries, threads);
        result.warnings.extend(warnings);
        Some(groups)
    } else {
        None
    };

    let reporter: Box<dyn Reporter> = match args.format {
        Format::Table => Box::new(TableReporter),
        Format::Tree => Box::new(TreeReporter),
        Format::Json => Box::new(JsonReporter),
    };
    reporter.report(&result, dups.as_deref(), args.top);

    ExitCode::SUCCESS
}
