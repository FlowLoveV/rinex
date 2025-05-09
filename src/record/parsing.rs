use crate::{
    antex::{
        record::{is_new_epoch as is_new_antex_epoch, parse_antenna as parse_antex_antenna},
        Record as AntexRecord,
    },
    clock::{
        record::{is_new_epoch as is_new_clock_epoch, parse_epoch as parse_clock_epoch},
        ClockKey, ClockProfile, Record as ClockRecord,
    },
    doris::{
        is_new_epoch as is_new_doris_epoch, parse_epoch as parse_doris_epoch, Record as DorisRecord,
    },
    epoch::parse_ionex_utc as parse_ionex_utc_epoch,
    hatanaka::DecompressorExpert,
    ionex::{
        is_new_height_map,
        is_new_rms_map,
        is_new_tec_map,
        //parse_height_map as parse_ionex_height_map,
        parse_rms_map as parse_ionex_rms_map,
        parse_tec_map as parse_ionex_tec_map,
        Quantized as IonexQuantized,
        Record as IonexRecord,
    },
    is_rinex_comment,
    meteo::{
        is_new_epoch as is_new_meteo_epoch, parse_epoch as parse_meteo_epoch, Record as MeteoRecord,
    },
    navigation::{
        is_new_epoch as is_new_nav_epoch, parse_epoch as parse_nav_epoch, Record as NavRecord,
    },
    observation::Observations,
    observation::{
        is_new_epoch as is_new_observation_epoch, parse_epoch as parse_observation_epoch,
        Record as ObservationRecord,
    },
    prelude::{Constellation, Epoch, Header, ParsingError, TimeScale},
    record::{Comments, Record},
    types::Type,
};

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read},
    str::from_utf8,
};

#[cfg(feature = "log")]
use log::error;

impl Record {
    /// Parses [Record] section by consuming [Reader] entirely.
    /// This requires reference to [Header] that was just parsed by consuming [Reader] until this point.
    pub fn parse<R: Read>(
        header: &mut Header,
        reader: &mut BufReader<R>,
    ) -> Result<(Self, Comments), ParsingError> {
        // eos reached: process pending buffer & exit
        let mut eos = false;

        // crinex decompression in failure: process pending buffer & exit
        let mut crinex_error = false;

        // current line storage
        let mut line_buf = String::with_capacity(128);

        // epoch storage
        let mut epoch_buf = String::with_capacity(1024);

        // comments management
        let mut comments: Comments = Comments::new();
        let mut comment_ts = Epoch::default();
        let mut comment_content = Vec::<String>::with_capacity(4);

        // ANTEX
        let mut atx_rec = AntexRecord::new();

        // NAV
        let mut nav_rec = NavRecord::new();

        // OBS
        let mut obs_rec = ObservationRecord::new();
        let mut observations = Observations::default();

        // CRINEX case
        const CRINEX_BUF_SIZE: usize = 1024;
        let mut buf = [0; CRINEX_BUF_SIZE];

        let mut is_crinex = false;
        let mut crinex_v3 = false;
        let mut gnss_observables = Default::default();

        if let Some(obs) = &header.obs {
            if let Some(crinex) = &obs.crinex {
                is_crinex = true;
                crinex_v3 = crinex.version.major > 2;
            }
            gnss_observables = obs.codes.clone();
        }

        // Build a decompressor, that we deployed if needed.
        // These parameters are compatible with historical RNX2CRX tool.
        let mut decompressor = DecompressorExpert::<5>::new(
            crinex_v3,
            header.constellation.unwrap_or_default(),
            gnss_observables,
        );

        // MET
        let mut met_rec = MeteoRecord::new();

        // CLK
        let mut clk_rec = ClockRecord::new();

        // DORIS
        let mut dor_rec = DorisRecord::new();

        // OBSERVATION case: timescale is either defined by
        // [+] TIME OF FIRST header field
        // [+] TIME OF LAST header field (flexibility, actually invalid according to specs)
        let mut obs_ts = TimeScale::default();

        if let Some(obs) = &header.obs {
            if let Some(t) = obs.timeof_first_obs {
                obs_ts = t.time_scale;
            } else {
                let t = obs
                    .timeof_last_obs
                    .ok_or(ParsingError::BadObsBadTimescaleDefinition)?;
                obs_ts = t.time_scale;
            }
        }

        // Clock RINEX TimeScale definition.
        // Modern revisions define it in header directly.
        // Old revisions are once again badly defined and most likely not thought out.
        //  + We default to GPST to "match" the case where this file is multi constellation
        //   and it seems that clocks steered to GPST is the most common case.
        //   For example NASA/CDDIS.com
        //  + In mono constellation, we adapt to that timescale.
        let mut clk_ts = TimeScale::GPST;
        if let Some(clk) = &header.clock {
            if let Some(ts) = clk.timescale {
                clk_ts = ts;
            } else {
                if let Some(constellation) = &header.constellation {
                    if let Some(ts) = constellation.timescale() {
                        clk_ts = ts;
                    }
                }
            }
        }

        // IONEX case
        //  Default map type is TEC, it will come with identified Epoch
        //  but others may exist:
        //  in this case we use the previously identified Epoch
        //  and attach other kinds of maps
        let mut ionex_rec = IonexRecord::new();
        let mut ionex_t = Epoch::default();
        let mut ionex_tec_exponent = Default::default();
        let mut ionex_lat_exponent = Default::default();
        let mut ionex_long_exponent = Default::default();
        let mut ionex_alt_exponent = Default::default();

        if let Some(ionex) = &header.ionex {
            ionex_tec_exponent = ionex.exponent;
            ionex_lat_exponent = IonexQuantized::find_exponent(ionex.grid.latitude.spacing);
            ionex_long_exponent = IonexQuantized::find_exponent(ionex.grid.longitude.spacing);
            ionex_alt_exponent = IonexQuantized::find_exponent(ionex.grid.height.spacing);
        }

        // Iterate and consume, one line at a time
        while let Ok(size) = reader.read_line(&mut line_buf) {
            if size == 0 {
                // reached EOS
                // we might still have something to process prior exiting
                eos |= true;
            }

            // (special case) COMMENTS: store as is
            if is_rinex_comment(&line_buf) {
                let comment = line_buf.split_at(60).0.trim_end();
                comment_content.push(comment.to_string());

                // skip parsing
                line_buf.clear();
                continue;
            }

            // (special case) IONEX exponent scaling:
            // keep up to date, so data interpretation remains correct
            if line_buf.contains("EXPONENT") {
                // we don't tolerate invalid scaling specs.
                // This assures that data interpretation is always correct.
                ionex_tec_exponent = line_buf
                    .split_at(60)
                    .0
                    .trim()
                    .parse::<i8>()
                    .map_err(|_| ParsingError::IonexScalingExponent)?;

                // skip parsing
                line_buf.clear();
                continue;
            }

            // (special case) IONEX temporal classification
            if line_buf.contains("EPOCH OF CURRENT MAP") {
                // we do not tolerate invalid temporal specs
                // This assures that data is always correctly sorted
                ionex_t = parse_ionex_utc_epoch(line_buf.split_at(60).0)?;

                // skip parsing
                line_buf.clear();
                continue;
            }

            // (special case) COMMENTS: store as is
            if line_buf.contains("COMMENT") {
                let content = line_buf.split_at(60).0.trim();
                if let Some(comments) = comments.get_mut(&comment_ts) {
                    comments.push(content.to_string());
                } else {
                    comments.insert(comment_ts, vec![content.to_string()]);
                }
            }

            // CRINEX special case:
            // - apply decompression algorithm prior moving forward
            // - decompress new pending line, which may recover several lines (in old V1 format)
            if is_crinex {
                let line_len = line_buf.len();

                // catch errors nicely, simply log them
                // it is normal to abort on final line for example
                match decompressor.decompress(&line_buf, line_len, &mut buf, CRINEX_BUF_SIZE) {
                    Ok(size) => {
                        if size > 0 {
                            // clear and overwrite pending content with recovered content
                            // we should have valid ASCII UTF-8 at all times, at this point
                            let recovered =
                                from_utf8(&buf[..size]).map_err(|_| ParsingError::BadUtf8Crinex)?;

                            line_buf.clear();
                            line_buf = recovered.to_string();
                            line_buf.push('\n');
                        }
                    },
                    Err(_) => {
                        crinex_error = true;
                    },
                }
            }

            let mut new_epoch = false;

            // we're trying to stack a complete epoch
            // that we process once a new one appears
            if epoch_buf.len() > 0 {
                new_epoch = Self::is_new_epoch(&line_buf, &header);

                // trick to force attempt on last iteration
                new_epoch |= eos;

                if new_epoch {
                    // new epoch appearing: process what we have buffered
                    // parsing method is format dependent
                    //println!("***MATCH***");

                    match &header.rinex_type {
                        Type::NavigationData => {
                            if let Ok((k, v)) = parse_nav_epoch(&header, &epoch_buf) {
                                nav_rec.insert(k, v);
                                // println!("nav_epoch={:?}", k); // DEBUG
                                comment_ts = k.epoch; // for comments storage
                            }
                        },
                        Type::ObservationData => {
                            match parse_observation_epoch(
                                header,
                                &epoch_buf,
                                obs_ts,
                                &mut observations,
                            ) {
                                Ok(key) => {
                                    //println!("key={:?}", key);
                                    obs_rec.insert(key, observations.clone());
                                    comment_ts = key.epoch; // for comments storage
                                },
                                #[cfg(feature = "log")]
                                Err(e) => {
                                    error!("parsing: {}", e);
                                },
                                #[cfg(not(feature = "log"))]
                                Err(_) => {},
                            }

                            observations.signals.clear(); // reset for next parsing (single alloc)
                        },

                        Type::DORIS => {
                            if let Ok((k, observations)) = parse_doris_epoch(header, &epoch_buf) {
                                comment_ts = k.epoch; // for comments storage
                                dor_rec.insert(k, observations);
                            }
                        },

                        Type::MeteoData => {
                            if let Ok(items) = parse_meteo_epoch(header, &epoch_buf) {
                                for (k, v) in items.iter() {
                                    met_rec.insert(k.clone(), *v);
                                    comment_ts = k.epoch; // for comments storage
                                }
                            }
                        },

                        Type::ClockData => {
                            if let Ok((epoch, key, profile)) =
                                parse_clock_epoch(header.version, &epoch_buf, clk_ts)
                            {
                                if let Some(e) = clk_rec.get_mut(&epoch) {
                                    e.insert(key, profile);
                                } else {
                                    let mut inner: BTreeMap<ClockKey, ClockProfile> =
                                        BTreeMap::new();
                                    inner.insert(key, profile);
                                    clk_rec.insert(epoch, inner);
                                }
                                comment_ts = epoch; // for comments storage
                            }
                        },

                        Type::AntennaData => {
                            let (antenna, content) = parse_antex_antenna(&epoch_buf).unwrap();
                            atx_rec.push((antenna, content));
                        },

                        Type::IonosphereMaps => {
                            if is_new_tec_map(&line_buf) {
                                match parse_ionex_tec_map(
                                    &epoch_buf,
                                    ionex_lat_exponent,
                                    ionex_long_exponent,
                                    ionex_alt_exponent,
                                    ionex_tec_exponent,
                                    ionex_t,
                                    &mut ionex_rec,
                                ) {
                                    Ok(()) => {},
                                    Err(_) => {},
                                }
                            } else if is_new_rms_map(&line_buf) {
                                match parse_ionex_rms_map(
                                    &epoch_buf,
                                    ionex_lat_exponent,
                                    ionex_long_exponent,
                                    ionex_alt_exponent,
                                    ionex_tec_exponent,
                                    ionex_t,
                                    &mut ionex_rec,
                                ) {
                                    Ok(()) => {},
                                    Err(_) => {},
                                }
                            } else {
                                // match parse_ionex_height_map(
                                //     &epoch_buf,
                                //     ionex_lat_exponent,
                                //     ionex_long_exponent,
                                //     ionex_alt_exponent,
                                //     ionex_tec_exponent,
                                //     ionex_t,
                                //     &mut ionex_rec,
                                // ) {
                                //     Ok(()) => {},
                                //     Err(_) => {},
                                // }
                            }
                        },
                    }
                }
            }

            // clear on new epoch detection
            if new_epoch {
                epoch_buf.clear();
            }

            // always stack new content
            epoch_buf.push_str(&line_buf);

            if eos || crinex_error {
                break;
            }

            line_buf.clear(); // always clear newline buf
        } //loop

        // wrap content and exit
        let record = match &header.rinex_type {
            Type::AntennaData => Record::AntexRecord(atx_rec),
            Type::ClockData => Record::ClockRecord(clk_rec),
            Type::IonosphereMaps => Record::IonexRecord(ionex_rec),
            Type::MeteoData => Record::MeteoRecord(met_rec),
            Type::NavigationData => Record::NavRecord(nav_rec),
            Type::ObservationData => Record::ObsRecord(obs_rec),
            Type::DORIS => Record::DorisRecord(dor_rec),
        };
        Ok((record, comments))
    }

    fn is_new_epoch(line: &str, header: &Header) -> bool {
        if is_rinex_comment(line) {
            return false;
        }
        match &header.rinex_type {
            Type::AntennaData => is_new_antex_epoch(line),
            Type::ClockData => is_new_clock_epoch(line),
            Type::IonosphereMaps => {
                is_new_tec_map(line) || is_new_rms_map(line) || is_new_height_map(line)
            },
            Type::NavigationData => is_new_nav_epoch(line, header.version),
            Type::ObservationData => is_new_observation_epoch(line, header.version),
            Type::MeteoData => is_new_meteo_epoch(line, header.version),
            Type::DORIS => is_new_doris_epoch(line),
        }
    }
}
