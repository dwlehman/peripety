use data::{EventType, ParserInfo};
use peripety::{BlkInfo, StorageEvent, StorageSubSystem};
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use std::thread::spawn;

fn parse_event(event: &StorageEvent, sender: &Sender<StorageEvent>) {
    let mut kdev: &str = &event.kdev;
    if event.kdev.starts_with("+scsi:host") {
        return;
    }
    if event.kdev.starts_with("+scsi:") {
        kdev = &event.kdev["+scsi:".len()..];
    }
    match BlkInfo::new_skip_extra(kdev) {
        Ok(b) => {
            let mut event = event.clone();
            event.dev_path = b.blk_path;
            event.dev_wwid = b.wwid;
            if event.event_type == "SCSI_SENSE_KEY" {
                if let Some(sense_key) = event.extension.get("sense_key") {
                    match sense_key.as_ref() {
                        // Find a way to use follow up CBD event to extract
                        // sector number of medium error.
                        "Medium Error" => {
                            event.event_type = "SCSI_MEDIUM_ERROR".to_string()
                        }
                        "Hardware Error" => {
                            event.event_type = "SCSI_HARDWARE_ERROR".to_string()
                        }
                        _ => {}
                    }
                }
            }
            event.msg =
                format!("{}, wwid: '{}'", event.raw_msg, event.dev_wwid);
            if let Err(e) = sender.send(event) {
                println!("scsi_parser: Failed to send event: {}", e);
            }
        }
        Err(e) => println!("scsi_parser: {}", e),
    }
}

pub fn parser_start(sender: Sender<StorageEvent>) -> ParserInfo {
    let (event_in_sender, event_in_recver) = mpsc::channel();
    let name = "scsi".to_string();
    let filter_event_type = vec![EventType::Raw];
    let filter_event_subsys = vec![StorageSubSystem::Scsi];

    spawn(move || loop {
        match event_in_recver.recv() {
            Ok(event) => parse_event(&event, &sender),
            Err(e) => println!("scsi_parser: Failed to receive event: {}", e),
        }
    });

    ParserInfo {
        sender: event_in_sender,
        name: name,
        filter_event_type: filter_event_type,
        filter_event_subsys: Some(filter_event_subsys),
    }
}
