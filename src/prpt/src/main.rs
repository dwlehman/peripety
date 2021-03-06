extern crate chrono;
#[macro_use]
extern crate clap;
extern crate nix;
extern crate peripety;
extern crate sdjournal;

use chrono::{DateTime, Local, TimeZone};
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};
use nix::sys::select::FdSet;
use peripety::{BlkInfo, LogSeverity, StorageEvent, StorageSubSystem};
use std::os::unix::io::AsRawFd;
use std::process::exit;

#[derive(Debug, Clone)]
struct CliOpt {
    severity: Option<LogSeverity>,
    sub_systems: Option<Vec<StorageSubSystem>>,
    event_types: Option<Vec<String>>,
    since: Option<u64>,
    blk_info: Option<BlkInfo>,
    is_json: bool,
}

arg_enum!{
    #[derive(Debug)]
    enum Severity {
        Emergency,
        Alert,
        Ctritical,
        Error,
        Warning,
        Notice,
        Info,
        Debug
    }
}

fn quit_with_msg(msg: &str) {
    println!("Error: {}", msg);
    exit(1);
}

fn arg_match_to_cliopt(matches: &ArgMatches) -> CliOpt {
    let mut ret = CliOpt {
        severity: None,
        sub_systems: None,
        event_types: None,
        since: None,
        blk_info: None,
        is_json: false,
    };
    if matches.is_present("severity") {
        match matches.value_of("severity") {
            Some(s) => {
                ret.severity = match s.parse::<LogSeverity>() {
                    Ok(s) => Some(s),
                    Err(e) => {
                        quit_with_msg(&format!("{}", e));
                        None
                    }
                }
            }
            None => quit_with_msg("Invalid severity"),
        }
    }
    if matches.is_present("event-type") {
        match matches.values_of("event-type") {
            Some(ets) => {
                let mut event_types = Vec::new();
                for et in ets {
                    event_types.push(et.to_string());
                }
                ret.event_types = Some(event_types);
            }
            None => quit_with_msg("Invalid event-type"),
        }
    }
    if matches.is_present("sub-system") {
        match matches.values_of("sub-system") {
            Some(subs) => {
                let mut sub_systems = Vec::new();
                for s in subs {
                    match s.parse::<StorageSubSystem>() {
                        Ok(s) => sub_systems.push(s),
                        Err(e) => quit_with_msg(&format!("{}", e)),
                    }
                }
                ret.sub_systems = Some(sub_systems);
            }
            None => quit_with_msg("Invalid sub-system"),
        }
    }
    if matches.is_present("since") {
        match matches.value_of("since") {
            Some(s) => match Local
                .datetime_from_str(&format!("{} 00:00:00", s), "%F %H:%M:%S")
            {
                Ok(t) => {
                    let timestamp = (t.timestamp() as u64) * 10u64.pow(6)
                        + t.timestamp_subsec_micros() as u64;
                    ret.since = Some(timestamp);
                }
                Err(e) => {
                    quit_with_msg(&format!("Failed to parse --since: {}", e))
                }
            },
            None => quit_with_msg("Invalid since"),
        }
    }

    ret.is_json = matches.is_present("J");

    if matches.is_present("blk") {
        match matches.value_of("blk") {
            Some(s) => {
                ret.blk_info = match BlkInfo::new_skip_extra(&s) {
                    Ok(b) => Some(b),
                    Err(e) => {
                        quit_with_msg(&format!("Invalid blk option: {}", e));
                        None
                    }
                };
            }
            None => quit_with_msg("Invalid blk option"),
        };
    }
    return ret;
}


// TODO(Gris Ge): If performance is a concern and moving search to journal API
//                add_match() could speed things up, we should do it.
//                Need investigation.
fn handle_event(event: &StorageEvent, cli_opt: &CliOpt) {
    let mut is_match = true;

    if let Some(ref l) = cli_opt.severity {
        if l < &event.severity {
            is_match = false;
        }
    }
    if let Some(ref subs) = cli_opt.sub_systems {
        if subs.len() != 0 && !subs.contains(&event.sub_system) {
            is_match = false;
        }
    }

    if let Some(ref ets) = cli_opt.event_types {
        if ets.len() != 0 && !ets.contains(&event.event_type) {
            is_match = false;
        }
    }

    if let Some(ref b) = cli_opt.blk_info {
        if event.dev_wwid != b.wwid && !event.owners_wwids.contains(&b.wwid) {
            is_match = false;
        }
    }

    if is_match {
        if cli_opt.is_json {
            println!(
                "{}",
                event
                    .to_json_string_pretty()
                    .expect("BUG: event.to_json_string_pretty()")
            );
        } else {
            let ts = DateTime::parse_from_rfc3339(&event.timestamp)
                .expect("BUG: DateTime::parse_from_rfc3339()")
                .with_timezone(&Local)
                .to_rfc2822();
            let mut msg = &event.raw_msg;
            if event.msg.len() != 0 {
                msg = &event.msg;
            }
            println!(
                "{} {} {} {}",
                ts, event.hostname, event.sub_system, msg
            )
        }
    }
}

fn handle_monitor(cli_opt: &CliOpt) {
    if let Some(_) = cli_opt.since {
        quit_with_msg("`monitor` sub-command does not allow `--since` option");
    }

    let mut journal =
        sdjournal::Journal::new().expect("Failed to open systemd journal");
    // We never want to block, so set the timeout to 0
    journal.timeout_us = 0;
    // Jump to the end as we cannot annotate old journal entries.
    journal
        .seek_tail()
        .expect("Unable to seek to end of journal!");
    journal
        .add_match("IS_PERIPETY=TRUE")
        .expect("Unable to search peripety journal");

    loop {
        let mut fds = FdSet::new();
        fds.insert(journal.as_raw_fd());
        if let Err(e) =
            nix::sys::select::select(None, Some(&mut fds), None, None, None)
        {
            println!(
                "collector: Failed select against journal fd: {}",
                e
            );
            continue;
        }
        if !fds.contains(journal.as_raw_fd()) {
            continue;
        }
        for entry in &mut journal {
            match entry {
                Ok(entry) => {
                    if let Some(j) = entry.get("JSON") {
                        if let Ok(event) = StorageEvent::from_json_string(j) {
                            handle_event(&event, &cli_opt);
                        }
                    }
                }
                Err(e) => {
                    println!("Error retrieving the journal entry: {:?}", e)
                }
            }
        }
    }
}

fn handle_query(cli_opt: &CliOpt) {
    let mut journal =
        sdjournal::Journal::new().expect("Failed to open systemd journal");
    // We never want to block, so set the timeout to 0
    journal.timeout_us = 0;
    journal
        .add_match("IS_PERIPETY=TRUE")
        .expect("Unable to search peripety journal");

    if let Some(since) = cli_opt.since {
        journal
            .seek_realtime_usec(since)
            .expect(&format!(
                "Unable to seek journal after {}",
                since
            ));
    }
    for entry in &mut journal {
        match entry {
            Ok(entry) => {
                if let Some(j) = entry.get("JSON") {
                    match StorageEvent::from_json_string(j) {
                        Ok(event) => handle_event(&event, &cli_opt),
                        Err(e) => println!("Error: {}", e),
                    };
                }
            }
            Err(e) => println!("Error retrieving the journal entry: {:?}", e),
        }
    }
}

fn handle_info(blk: &str, is_json: bool) {
    match BlkInfo::new(blk) {
        Ok(i) => {
            if is_json {
                println!(
                    "{}",
                    i.to_json_string_pretty()
                        .expect("BUG: handle_info()")
                );
            } else {
                println!("blk_path     : {}", i.blk_path);
                println!("blk_type     : {}", i.blk_type);
                println!("wwid         : {}", i.wwid);
                println!("owners_wwids : {:?}", i.owners_wwids);
                println!("owners_paths : {:?}", i.owners_paths);
                let mut types = Vec::new();
                for t in i.owners_types {
                    types.push(format!("{}", t));
                }
                println!("owners_types : {:?}", types);
                println!(
                    "uuid         : {}",
                    i.uuid.unwrap_or("".to_string())
                );
                println!(
                    "mount_point  : {}",
                    i.mount_point.unwrap_or("".to_string())
                );
            }
        }
        Err(e) => quit_with_msg(&format!("{}", e)),
    };
}

fn main() {
    let sev_arg = Arg::from_usage(
        "--severity=[SEVERITY] 'Only show event with equal or higher severity'",
    ).possible_values(&Severity::variants())
        .case_insensitive(true)
        .default_value("Debug");

    let evt_arg = Arg::from_usage(
        "--event-type=[EVENT-TYPE]... \
         'Only show event with specific event type, argument could be \
         repeated'",
    );
    let sub_arg = Arg::from_usage(
        "--sub-system=[SUB-SYSTEM]... \
         'Only show event with specific sub-system, argument could be \
         repeated'",
    );
    let blk_arg =
        Arg::from_usage("--blk=[BLOCK] 'Only show event with specific block'");

    let json_arg = Arg::from_usage("-J 'Use json format'");

    let matches = App::new("Peripety CLI")
        .version("0.1")
        .author("Gris Ge <fge@redhat.com>")
        .about("CLI utile for peripety events")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .subcommand(
            SubCommand::with_name("monitor")
                .about("Monitor following up events")
                .arg(&json_arg)
                .arg(&sev_arg)
                .arg(&evt_arg)
                .arg(&sub_arg)
                .arg(&blk_arg),
        )
        .subcommand(
            SubCommand::with_name("query")
                .about("Query saved events")
                .arg(&json_arg)
                .arg(&sev_arg)
                .arg(&evt_arg)
                .arg(&sub_arg)
                .arg(&blk_arg)
                .arg(Arg::from_usage(
                    "--since [SINCE] \
                     'Only show event on or newer than the specified \
                     date, format is ISO 8601: 2018-05-21'",
                )),
        )
        .subcommand(
            SubCommand::with_name("info")
                .about("Query block information")
                .arg(Arg::from_usage(
                    "<blk> 'Block to query, could be \'major:minor\', \
                     block name, block path, symbolic link to block, \
                     uuid, wwid, or fs mount point'",
                ))
                .arg(&json_arg),
        )
        .get_matches();

    if let Some(matches) = matches.subcommand_matches("monitor") {
        let cli_opt = arg_match_to_cliopt(&matches);
        handle_monitor(&cli_opt);
        exit(0);
    }

    if let Some(matches) = matches.subcommand_matches("query") {
        let cli_opt = arg_match_to_cliopt(&matches);
        handle_query(&cli_opt);
        exit(0);
    }

    if let Some(matches) = matches.subcommand_matches("info") {
        let is_json = matches.is_present("J");
        match matches.value_of("blk") {
            Some(s) => handle_info(s, is_json),
            None => quit_with_msg("Invalid 'blk' argument"),
        }
        exit(0);
    }
}
