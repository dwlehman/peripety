// Collector is supposed to get log from systemd journal and generate
// event with kdev and sub system type.

// Many code are copied from Tony's
// https://github.com/tasleson/storage_event_monitor/blob/master/src/main.rs
// Which is MPL license.

use chrono::{Local, SecondsFormat, TimeZone};
use nix;
use nix::sys::select::FdSet;
use peripety::{LogSeverity, StorageEvent, StorageSubSystem};
use sdjournal;
use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::{Receiver, Sender};

use buildin_regex::BUILD_IN_REGEX_CONFS;
use conf::ConfCollector;
use data::RegexConf;

fn process_journal_entry(
    entry: &HashMap<String, String>,
    sender: &Sender<StorageEvent>,
    buildin_regex_confs: &Vec<RegexConf>,
    user_regex_confs: &Vec<RegexConf>,
) {
    let msg = match entry.get("MESSAGE") {
        Some(m) => {
            if m.len() == 0 {
                return;
            }
            m
        }
        None => return,
    };

    if !entry.contains_key("SYSLOG_IDENTIFIER") {
        return;
    }

    // Skip messages generated by peripetyd.
    if entry.get("IS_PERIPETY") == Some(&"TRUE".to_string()) {
        return;
    }

    // The /dev/kmsg can hold userspace log, hence using `_TRANSPORT=kernel` is
    // not correct here.
    if entry.get("SYSLOG_IDENTIFIER") != Some(&"kernel".to_string()) {
        return;
    }

    let mut event: StorageEvent = Default::default();

    // Currently, SCSI layer have limited structured log holding
    // device and subsystem type, but without regex, we cannot know the
    // event type. Hence we do regex anyway without checking structured log,
    // we can do that when kernel provide better structured log.

    if let Some(s) = entry.get("_KERNEL_SUBSYSTEM") {
        match s.parse::<StorageSubSystem>() {
            Ok(s) => event.sub_system = s,
            Err(e) => println!("collector: {}", e),
        }
    }
    if let Some(d) = entry.get("_KERNEL_DEVICE") {
        event.kdev = d.to_string();
    }

    for regex_conf in buildin_regex_confs
        .iter()
        .chain(user_regex_confs.iter())
    {
        // Save CPU if event.sub_system is defined and not matching with regex
        // config.
        if event.sub_system != StorageSubSystem::Unknown
            && regex_conf.sub_system != event.sub_system
        {
            continue;
        }
        // Save CPU from regex.captures() if starts_with() failed.
        if let Some(ref s) = regex_conf.starts_with {
            if !msg.starts_with(s) {
                continue;
            }
        }
        if let Some(cap) = regex_conf.regex.captures(msg) {
            if let Some(m) = cap.name("kdev") {
                event.kdev = m.as_str().to_string();
            }
            if event.kdev.len() == 0 {
                continue;
            }

            if regex_conf.sub_system != StorageSubSystem::Unknown {
                event.sub_system = regex_conf.sub_system;
            }

            if regex_conf.event_type.len() != 0 {
                event.event_type = regex_conf.event_type.to_string();
            }

            // If regex has other named group, we save it to event.extension.
            for name in regex_conf.regex.capture_names() {
                if let Some(name) = name {
                    if name == "kdev" {
                        continue;
                    }
                    if let Some(m) = cap.name(name) {
                        event
                            .extension
                            .insert(name.to_string(), m.as_str().to_string());
                    }
                }
            }

            break;
        }
    }

    if event.sub_system == StorageSubSystem::Unknown || event.kdev.len() == 0 {
        return;
    }

    // Add other data
    event.hostname = entry
        .get("_HOSTNAME")
        .unwrap_or(&"".to_string())
        .to_string();

    if let Some(t) = entry.get("__REALTIME_TIMESTAMP") {
        let tp = match t.parse::<i64>() {
            Ok(t) => t,
            Err(_) => return,
        };
        event.timestamp = Local
            .timestamp(tp / 10i64.pow(6), (tp % 10i64.pow(6)) as u32)
            .to_rfc3339_opts(SecondsFormat::Micros, false)
    } else {
        return;
    }

    if let Some(p) = entry.get("PRIORITY") {
        event.severity = match p.parse::<LogSeverity>() {
            Ok(s) => s,
            Err(e) => {
                println!("collector: {}", e);
                LogSeverity::Unknown
            }
        }
    }

    event.raw_msg = msg.to_string();
    //TODO(Gris Ge): Generate event_id here.

    //TODO(Gris Ge): Need to skip journal entry when that one is created by
    //               peripety.
    if let Err(e) = sender.send(event) {
        println!("collector: Failed to send event: {}", e);
    }
}

pub fn new(
    sender: &Sender<StorageEvent>,
    config_changed: &Receiver<ConfCollector>,
) {
    let mut journal =
        sdjournal::Journal::new().expect("Failed to open systemd journal");
    // We never want to block, so set the timeout to 0
    journal.timeout_us = 0;
    // Jump to the end as we cannot annotate old journal entries.
    journal
        .seek_tail()
        .expect("Unable to seek to end of journal!");

    // Setup initial regex conf.
    let mut buildin_regex_confs: Vec<RegexConf> = Vec::new();
    let mut user_regex_confs: Vec<RegexConf> = Vec::new();

    // Read config to add more RegexConf.
    for regex_conf_str in BUILD_IN_REGEX_CONFS {
        let regex_conf = regex_conf_str.to_regex_conf();
        buildin_regex_confs.push(regex_conf);
    }

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
                    if let Ok(conf) = config_changed.try_recv() {
                        user_regex_confs.clear();
                        for regex in conf.regexs {
                            match regex.to_regex_conf() {
                                Ok(r) => user_regex_confs.push(r),
                                Err(e) => {
                                    println!(
                                        "collector: Invalid config: {}",
                                        e
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                    process_journal_entry(
                        &entry,
                        sender,
                        &buildin_regex_confs,
                        &user_regex_confs,
                    )
                }
                Err(e) => {
                    println!("Error retrieving the journal entry: {:?}", e)
                }
            }
        }
    }
}
