use data::{BlkInfo, BlkType, EventType, ParserInfo, Sysfs};
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use peripety::{StorageEvent, StorageSubSystem};
use std::thread::Builder;
use std::fs;
use std::collections::HashMap;
use regex::Regex;
use std::path::Path;

//TODO(Gris Ge): Maybe we should be save iscsi/fc data into BlkInfo::new().
fn iscsi_session_id_of_host(host_id: &str) -> Option<String> {
    let path = format!("/sys/class/iscsi_host/host{}", host_id);
    let p = match fs::read_link(&path) {
        Ok(l) => match l.to_str() {
            Some(s) => s.to_string(),
            None => {
                println!("mpath_parser: Got non-unicode: {:?}", path);
                return None;
            }
        },
        Err(e) => {
            println!("mpath_parser: Error when read_link {}: {}", path, e);
            return None;
        }
    };
    let dev_path = match Regex::new(r"\.+/(devices/.+/host[0-9]+)/iscsi_host/")
        .expect("BUG")
        .captures(&p)
    {
        Some(c) => {
            match c.get(1) {
                Some(d) => format!("/sys/{}", d.as_str()),
                None => {
                    // should never happen, if does, silently return.
                    return None;
                }
            }
        }
        None => {
            println!("mpath_parser: Failed to do regex parsing on {}", p);
            return None;
        }
    };

    let dir_entries = match fs::read_dir(&dev_path) {
        Ok(b) => b,
        Err(e) => {
            println!("mpath_parser: Failed to read_dir {}: {}", dev_path, e);
            return None;
        }
    };
    for e in dir_entries {
        if let Ok(e) = e {
            if let Ok(name) = e.file_name().into_string() {
                if name.starts_with("session") {
                    return Some(name["session".len()..].to_string());
                }
            }
        }
    }
    None
}

fn get_iscsi_host_info(host_id: &str) -> HashMap<String, String> {
    let mut ret = HashMap::new();
    if let Some(sid) = iscsi_session_id_of_host(host_id) {
        let session_dir = format!("/sys/class/iscsi_session/session{}", sid);
        let conn_dir =
            format!("/sys/class/iscsi_connection/connection{}:0", sid);
        if !Path::new(&session_dir).exists() {
            return ret;
        }
        if !Path::new(&conn_dir).exists() {
            return ret;
        }
        ret.insert(
            "address".to_string(),
            Sysfs::read(&format!("{}/{}", conn_dir, "address")),
        );
        ret.insert(
            "port".to_string(),
            Sysfs::read(&format!("{}/{}", conn_dir, "port")),
        );
        ret.insert(
            "tpgt".to_string(),
            Sysfs::read(&format!("{}/{}", session_dir, "tpgt")),
        );
        ret.insert(
            "target_name".to_string(),
            Sysfs::read(&format!("{}/{}", session_dir, "targetname")),
        );
        ret.insert(
            "iface_name".to_string(),
            Sysfs::read(&format!("{}/{}", session_dir, "ifacename")),
        );
    }

    ret
}

fn fc_host_id_of_host(host_id: &str) -> Option<String> {
    None
}

fn get_fc_host_info(host_id: &str) -> HashMap<String, String> {
    let ret = HashMap::new();
    ret
}

fn is_iscsi_host(host_id: &str) -> bool {
    Path::new(&format!("/sys/class/iscsi_host/host{}", host_id)).exists()
}

fn is_fc_host(host_id: &str) -> bool {
    Path::new(&format!("/sys/class/fc_host/host{}", host_id)).exists()
}

fn get_scsi_host_info(blk_name: &str) -> HashMap<String, String> {
    let mut ret = HashMap::new();
    let host_id = match Sysfs::scsi_host_id_of_disk(blk_name) {
        Some(h) => h,
        None => return ret,
    };
    ret.insert(
        "driver_name".to_string(),
        Sysfs::read(&format!(
            "/sys/class/scsi_host/host{}/proc_name",
            &host_id
        )),
    );
    if is_iscsi_host(&host_id) {
        ret.insert("transport".to_string(), "iSCSI".to_string());
        for (key, value) in get_iscsi_host_info(&host_id) {
            ret.insert(key, value);
        }
    } else if is_fc_host(&host_id) {
        ret.insert("transport".to_string(), "FC".to_string());
        for (key, value) in get_fc_host_info(&host_id) {
            ret.insert(key, value);
        }
    }

    ret
}

fn get_mpath_info_from_blk(major_minor: &str) -> Option<(String, String)> {
    // We use sysfs information to speed up things without cacheing.
    // TODO(Gris Ge): This function should return Result<>
    let sysfs_holder_dir = format!("/sys/dev/block/{}/holders", major_minor);
    let mut holders = match fs::read_dir(&sysfs_holder_dir) {
        Ok(o) => o,
        Err(e) => {
            println!(
                "mpath_parser: Failed to read_dir {}: {}",
                sysfs_holder_dir, e
            );
            return None;
        }
    };
    match holders.next() {
        Some(Ok(holder)) => {
            let dm = holder.path();
            let dm = match dm.to_str() {
                Some(p) => p,
                None => {
                    println!(
                        "mpath_parser: Path {:?} is not valid unicode",
                        holder
                    );
                    return None;
                }
            };
            let name_path = format!("{}/dm/name", dm);
            let uuid_path = format!("{}/dm/uuid", dm);
            let mut uuid = Sysfs::read(&uuid_path);
            if uuid.starts_with("mpath-") {
                uuid = uuid["mpath-".len()..].to_string();
                return Some((Sysfs::read(&name_path), uuid));
            }
        }
        Some(Err(e)) => println!(
            "mpath_parser: Failed to read_dir {}: {}",
            sysfs_holder_dir, e
        ),
        None => println!("mpath_parser: {} is empty", sysfs_holder_dir),
    };
    None
}

fn parse_event(event: &StorageEvent, sender: &Sender<StorageEvent>) {
    match event.event_type.as_ref() {
        "DM_MPATH_PATH_FAILED" | "DM_MPATH_PATH_REINSTATED" => {
            let (name, uuid) = match get_mpath_info_from_blk(&event.kdev) {
                Some(t) => t,
                None => return,
            };
            let mut event = event.clone();
            event.dev_path = format!("/dev/mapper/{}", name);
            event.dev_name = name;
            event.dev_wwid = uuid;
            if let Some(blk_info) = BlkInfo::new(&event.kdev) {
                event.owners_wwids.push(blk_info.wwid.clone());
                event.owners_names.push(blk_info.name.clone());
                event.owners_paths.push(blk_info.blk_path.clone());
                if blk_info.blk_type == BlkType::Scsi {
                    // Check for iSCSI/FC/FCoE informations.
                    for (key, value) in get_scsi_host_info(&blk_info.name) {
                        event.extention.insert(key, value);
                    }
                }
            }
            event
                .extention
                .insert("blk_major_minor".to_string(), event.kdev.clone());
            if let Err(e) = sender.send(event) {
                println!("mpath_parser: Failed to send event: {}", e);
            }
        }
        _ => println!("mpath: Got unknown event type: {}", event.event_type),
    };
}

pub fn parser_start(sender: Sender<StorageEvent>) -> ParserInfo {
    let (event_in_sender, event_in_recver) = mpsc::channel();
    let name = "mpath".to_string();
    let filter_event_type = vec![EventType::Raw];
    let filter_event_subsys = vec![StorageSubSystem::Multipath];

    if let Err(e) = Builder::new().name("mpath_parser".into()).spawn(
        move || loop {
            match event_in_recver.recv() {
                Ok(event) => parse_event(&event, &sender),
                Err(e) => {
                    println!("mpath_parser: Failed to retrieve event: {}", e)
                }
            };
        },
    ) {
        panic!("mpath_parser: Failed to create parser thread: {}", e);
    }

    println!("mpath_parser: Ready");
    ParserInfo {
        sender: event_in_sender,
        name: name,
        filter_event_type: filter_event_type,
        filter_event_subsys: Some(filter_event_subsys),
    }
}
