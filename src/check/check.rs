use crate::db::models;
use crate::db::models::StationItem;
use crate::db::models::StationCheckItemNew;

use crate::thread;
use threadpool::ThreadPool;

use av_stream_info_rust;
use crate::check::favicon;

use std;
use std::sync::mpsc::channel;
use std::sync::mpsc::TryRecvError;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::db::DbConnection;
use crate::db::connect;

use colored::*;

#[derive(Clone,Debug)]
pub struct StationOldNew {
    pub old: StationItem,
    pub new: StationCheckItemNew,
}

fn check_for_change(
    old: &models::StationItem,
    new: &StationCheckItemNew,
    new_favicon: &str,
) -> (bool, String) {
    let mut retval = false;
    let mut result = String::from("");

    if old.lastcheckok != new.check_ok {
        if new.check_ok {
            result.push('+');
            result.red();
        } else {
            result.push('-');
        }
        retval = true;
    } else {
        result.push('~');
    }
    result.push(' ');
    result.push('\'');
    result.push_str(&old.name);
    result.push('\'');
    result.push(' ');
    result.push_str(&old.url);
    if old.hls != new.hls {
        result.push_str(&format!(" hls:{}->{}", old.hls, new.hls));
        retval = true;
    }
    if old.bitrate != new.bitrate {
        result.push_str(&format!(" bitrate:{}->{}", old.bitrate, new.bitrate));
        retval = true;
    }
    if old.codec != new.codec {
        result.push_str(&format!(" codec:{}->{}", old.codec, new.codec));
        retval = true;
    }
    /*if old.urlcache != new.url{
        debug!("  url      :{}->{}",old.urlcache,new.url);
        retval = true;
    }*/
    if old.favicon != new_favicon {
        result.push_str(&format!(" favicon: {} -> {}", old.favicon, new_favicon));
        retval = true;
    }
    if old.lastcheckok != new.check_ok {
        if new.check_ok {
            return (retval, result.green().to_string());
        } else {
            return (retval, result.red().to_string());
        }
    } else {
        return (retval, result.yellow().to_string());
    }
}

fn update_station(
    conn: &Box<dyn DbConnection>,
    old: &models::StationItem,
    new_item: StationCheckItemNew,
    new_favicon: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // output debug
    let (changed, change_str) = check_for_change(&old, &new_item, new_favicon);
    if changed {
        debug!("{}", change_str.red());
    } else {
        debug!("{}", change_str.dimmed());
    }

    // do real insert
    let list_new = vec!(new_item);
    let (_x,_y,inserted) = conn.insert_checks(list_new)?;
    conn.update_station_with_check_data(&inserted, true)?;
    Ok(())
}

fn dbcheck_internal(
    pool: &ThreadPool,
    stations: Vec<StationItem>,
    source: &str,
    timeout: u64,
    max_depth: u8,
    retries: u8,
    result_sender: Sender<StationOldNew>,
) -> u32 {
    let mut checked_count = 0;
    for station in stations {
        checked_count = checked_count + 1;
        let source = String::from(source);
        let result_sender = result_sender.clone();
        pool.execute(move || {
            {
                let (_, receiver): (Sender<i32>, Receiver<i32>) = channel();
                let station_name = station.name.clone();
                let max_timeout = (retries as u64) * timeout * 2;
                thread::spawn(move || {
                    for _ in 0..max_timeout {
                        thread::sleep(Duration::from_secs(1));
                        let o = receiver.try_recv();
                        match o {
                            Ok(_) => {
                                return;
                            }
                            Err(value) => match value {
                                TryRecvError::Empty => {}
                                TryRecvError::Disconnected => {
                                    return;
                                }
                            },
                        }
                    }
                    debug!("Still not finished: {}", station_name);
                    std::process::exit(0x0100);
                });
            }

            let mut items = av_stream_info_rust::check(&station.url, timeout as u32, max_depth, retries);
            for item in items.drain(..) {
                match item {
                    Ok(item) => {
                        let override_metadata = item.OverrideIndexMetaData.unwrap_or(false);
                        let do_not_index = item.DoNotIndex.unwrap_or(false);
                        if do_not_index && override_metadata {
                            // ignore non public streams
                            debug!("Ignore private stream: {} - {}", station.stationuuid, item.Url);
                        }else{
                            let mut codec = item.CodecAudio.clone();
                            if let Some(ref video) = item.CodecVideo {
                                codec.push_str(",");
                                codec.push_str(&video);
                            }
                            let new_item_ok = StationCheckItemNew {
                                checkuuid: None,
                                station_uuid: station.stationuuid.clone(),
                                source: source.clone(),
                                codec: codec,
                                bitrate: item.Bitrate.unwrap_or(0),
                                hls: item.Hls,
                                check_ok: true,
                                url: item.Url.clone(),
                                timestamp: None,

                                metainfo_overrides_database: override_metadata,
                                public: item.Public,
                                name: item.Name,
                                description: item.Description,
                                tags: item.Genre,
                                countrycode: item.CountryCode,
                                homepage: item.Homepage,
                                favicon: item.LogoUrl,
                                loadbalancer: item.MainStreamUrl,
                                do_not_index: item.DoNotIndex,
                            };
                            let send_result = result_sender.send(StationOldNew {
                                old: station,
                                new: new_item_ok,
                            });
                            if let Err(send_result) = send_result {
                                error!("Unable to send positive check result: {}", send_result);
                            }
                            return;
                        }
                    }
                    Err(_) => {}
                }
            }
            let new_item_broken: StationCheckItemNew = StationCheckItemNew {
                checkuuid: None,
                station_uuid: station.stationuuid.clone(),
                source: source.clone(),
                codec: "".to_string(),
                bitrate: 0,
                hls: false,
                check_ok: false,
                url: "".to_string(),
                timestamp: None,
                
                metainfo_overrides_database: false,
                public: None,
                name: None,
                description: None,
                tags: None,
                countrycode: None,
                homepage: None,
                favicon: None,
                loadbalancer: None,
                do_not_index: None,
            };
            let send_result = result_sender.send(StationOldNew {
                old: station,
                new: new_item_broken,
            });
            if let Err(send_result) = send_result {
                error!("Unable to send negative check result: {}", send_result);
            }
        });
    }
    checked_count
}

pub fn dbcheck(
    connection_str: String,
    source: &str,
    concurrency: usize,
    stations_count: u32,
    useragent: &str,
    timeout: u64,
    max_depth: u8,
    retries: u8,
    favicon_checks: bool,
) -> Result<u32, Box<dyn std::error::Error>> {
    let mut conn = connect(connection_str)?;
    let stations = conn.get_stations_to_check(24, stations_count)?;
    let useragent = String::from(useragent);

    let (result_sender, result_receiver): (Sender<StationOldNew>, Receiver<StationOldNew>) =
        channel();
    let pool = ThreadPool::new(concurrency);
    let checked_count = dbcheck_internal(&pool, stations, source, timeout, max_depth, retries, result_sender);
    pool.join();

    for oldnew in result_receiver {
        let station = oldnew.old;
        let new_item = oldnew.new;
        if favicon_checks {
            let new_favicon = favicon::check(&station.homepage, &station.favicon, &useragent, timeout as u32)?;
            update_station(&mut conn, &station, new_item, &new_favicon)?;
        } else {
            update_station(&mut conn, &station, new_item, &station.favicon)?;
        }
    }
    Ok(checked_count)
}
