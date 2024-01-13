use std::env::current_dir;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use osus::algos::{
    convert_slider_points_to_legacy, mix_volume, offset_map, remove_duplicates,
    remove_useless_speed_changes, reset_hitsounds,
};
use osus::close_range;
use osus::file::beatmap::{
    BeatmapFile, HitObject, HitObjectParams, HitSample, HitSampleSet, HitSound, SampleBank,
    SliderPoint, TimingPoint,
};
use osus::{ExtTimestamped, Timestamped, TimestampedSlice};
use tracing::Level;
use walkdir::WalkDir;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

const PATH_HELP: &str = "Path to beatmap file or folder containing beatmap files.";

#[derive(Subcommand)]
enum Commands {
    /// Extract every .osu file from hashed osu!lazer files.
    ExtractOsuLazerFiles {
        #[arg(
            short,
            long,
            help = "Output path where to copy the beatmaps (defaults to ./maps/)."
        )]
        out_path: Option<PathBuf>,

        #[arg(
            short,
            long,
            help = "Whether to recurse in the folder. (option is ignored if the path is a file)."
        )]
        recursive: bool,

        #[arg(help = PATH_HELP)]
        path: PathBuf,
    },

    /// Offset the whole beatmap by some amount of milliseconds.
    Offset {
        #[arg(help = "Amount of milliseconds to offset the beatmap (can be a decimal number).")]
        millis: f64,

        #[arg(help = PATH_HELP)]
        path: PathBuf,
    },

    /// Raise or lower the beatmap's volume.
    MixVolume {
        #[arg(long, help = "Amount of volume to add. Can be positive or negative.")]
        val: i8,

        #[arg(help = PATH_HELP)]
        path: PathBuf,
    },

    /// Reset all hitsounds to the same sample set (not touching actual samples on hit objects).
    ResetSampleSets {
        #[arg(
            long,
            default_value_t = true,
            help = "Whether to use the Soft sample set as the overwriting value."
        )]
        soft: bool,

        #[arg(
            long,
            default_value_t = true,
            help = "Whether to cleanup timing points after resetting hitsounds."
        )]
        cleanup: bool,

        #[arg(help = PATH_HELP)]
        path: PathBuf,
    },

    /// Cleanup timing points by removing all the ones that are useless/duplicates.
    CleanupTimingPoints {
        #[arg(help = PATH_HELP)]
        path: PathBuf,
    },

    /// Take hitsounds from a map and splat them on another.
    SplatHitsounds {
        #[arg(short, long, help = "Path to hitsound map file.")]
        sound_map: PathBuf,

        #[arg(help = PATH_HELP)]
        path: PathBuf,

        #[arg(
            short,
            long,
            help = "Whether we're hitsounding for mania. In that case, an extra transformation happens to spread out hitsounds on all notes in each row as much as possible."
        )]
        mania: bool,
    },

    /// Convert a Lazer map (v128) to a Stable map (v14).
    LazerToStable {
        #[arg(help = PATH_HELP)]
        path: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    let Cli { command } = Cli::parse();

    match command {
        Commands::ExtractOsuLazerFiles {
            out_path,
            recursive,
            path,
        } => {
            let out_path = out_path.unwrap_or(current_dir()?.join("maps"));
            cli_extract_osu_lazer_files(&out_path, recursive, &path)?;
        }

        Commands::Offset { millis, path } => cli_offset(millis, &path)?,

        Commands::MixVolume { val, path } => cli_mix_volume(val, &path)?,

        Commands::ResetSampleSets {
            soft,
            cleanup,
            path,
        } => cli_reset_sample_sets(soft, cleanup, &path)?,

        Commands::CleanupTimingPoints { path } => cli_cleanup_timing_points(&path)?,

        Commands::SplatHitsounds {
            sound_map,
            path,
            mania,
        } => cli_splat_hitsounds(&sound_map, &path, mania)?,

        Commands::LazerToStable { path } => cli_lazer_to_stable(&path)?,
    }

    Ok(())
}

fn backup(path: &Path) -> io::Result<u64> {
    let mut out_path = path.with_extension("osu.backup");

    let mut n: u32 = 1;
    while out_path.exists() {
        out_path = path.with_extension(format!("osu.{n}.backup"));
        n += 1;
    }

    fs::copy(path, out_path)
}

fn parse_beatmap(path: &Path, do_backup: bool) -> Result<BeatmapFile, Box<dyn Error>> {
    if do_backup {
        tracing::warn!("Backing up {}...", path.display());
        backup(path)?;
    }

    tracing::warn!("Parsing {}...", path.display());
    let beatmap = BeatmapFile::parse(path).map_err(|e| e.to_string())?;

    Ok(beatmap)
}

fn write_beatmap_out(beatmap: &BeatmapFile, path: &Path) -> io::Result<()> {
    tracing::warn!("Write beatmap to {}...", path.display());
    let mut out_file = File::create(path)?;
    beatmap.deserialize(&mut out_file)?;

    Ok(())
}

fn cleanup_timing_points(beatmap: &mut BeatmapFile) {
    tracing::warn!("Removing duplicates...");
    beatmap.timing_points = remove_duplicates(&beatmap.timing_points);

    tracing::warn!("Removing useless speed changes...");
    beatmap.timing_points =
        remove_useless_speed_changes(&beatmap.timing_points, &beatmap.hit_objects);

    tracing::warn!("Removing duplicates again...");
    beatmap.timing_points = remove_duplicates(&beatmap.timing_points);
}

/// Combine and merge the hitsound information of a bunch of hitobjects into another one.
fn hitsound_hit_object(ho: &mut HitObject, ho_sounds: &[HitObject]) {
    for so in ho_sounds {
        tracing::info!("affecting {} at {}", ho.object_type, ho.timestamp());

        if so.hit_sample.normal_set != SampleBank::Auto {
            ho.hit_sample.normal_set = so.hit_sample.normal_set;
        }

        if so.hit_sample.addition_set != SampleBank::Auto {
            ho.hit_sample.addition_set = so.hit_sample.addition_set;
        }

        ho.hit_sample.index = so.hit_sample.index;
        ho.hit_sample.volume = so.hit_sample.volume;

        if so.hit_sample.filename.is_some() {
            ho.hit_sample.filename = so.hit_sample.filename.clone();
        }

        ho.hit_sound |= so.hit_sound;
    }
}

fn cli_extract_osu_lazer_files(
    out_path: &Path,
    recursive: bool,
    path: &Path,
) -> Result<(), Box<dyn Error>> {
    for entry in WalkDir::new(path)
        .max_depth(if recursive { usize::MAX } else { 0 })
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| !e.path().is_dir())
    {
        let file = File::open(entry.path())?;

        let mut buffer = BufReader::new(file);
        let mut first_line = String::new();
        let _ = buffer.read_line(&mut first_line);

        if first_line.starts_with("osu file format v") {
            println!("Map in {:?}", entry.path());
            let entry_out_path = Path::new(entry.file_name()).with_extension("osu");
            fs::copy(entry.path(), out_path.join(entry_out_path))?;
        }
    }

    Ok(())
}

fn cli_offset(millis: f64, path: &Path) -> Result<(), Box<dyn Error>> {
    let mut beatmap = parse_beatmap(path, true)?;

    tracing::warn!("Offsetting beatmap...");
    offset_map(&mut beatmap, millis);

    write_beatmap_out(&beatmap, path)?;
    Ok(())
}

fn cli_mix_volume(val: i8, path: &Path) -> Result<(), Box<dyn Error>> {
    let mut beatmap = parse_beatmap(path, true)?;

    tracing::warn!("Mixing volume...");
    mix_volume(&mut beatmap.timing_points, val);

    write_beatmap_out(&beatmap, path)?;
    Ok(())
}

fn cli_reset_sample_sets(soft: bool, cleanup: bool, path: &Path) -> Result<(), Box<dyn Error>> {
    let mut beatmap = parse_beatmap(path, true)?;

    tracing::warn!("Resetting hitsounds...");
    let sample_bank = if soft {
        SampleBank::Soft
    } else {
        SampleBank::Auto
    };
    reset_hitsounds(&mut beatmap.timing_points, sample_bank);

    if cleanup {
        cleanup_timing_points(&mut beatmap);
    }

    write_beatmap_out(&beatmap, path)?;
    Ok(())
}

fn cli_cleanup_timing_points(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut beatmap = parse_beatmap(path, true)?;

    cleanup_timing_points(&mut beatmap);

    write_beatmap_out(&beatmap, path)?;
    Ok(())
}

fn cli_splat_hitsounds(
    soundmap_path: &Path,
    beatmap_path: &Path,
    is_mania: bool,
) -> Result<(), Box<dyn Error>> {
    let mut beatmap = parse_beatmap(beatmap_path, true)?;
    let soundmap = parse_beatmap(soundmap_path, false)?;

    // reset beatmap's hitsounds
    tracing::warn!("Resetting beatmap's hitsounds...");
    for hit_object in &mut beatmap.hit_objects {
        hit_object.hit_sample = HitSample::default();
        hit_object.hit_sound = HitSound::NONE;

        if let HitObjectParams::Slider {
            edge_hitsounds,
            edge_samplesets,
            ..
        } = &mut hit_object.object_params
        {
            for eh in edge_hitsounds {
                *eh = HitSound::NONE;
            }

            for es in edge_samplesets {
                *es = HitSampleSet::default();
            }
        }
    }

    // insert soundmap's hitsound information from timing points
    tracing::warn!("Inserting soundmap's timing points...");
    let mut new_timing_points: Vec<TimingPoint> = Vec::new();
    let mut last_sound_point = &soundmap.timing_points[0];
    for smtp_bmtp in (soundmap.timing_points).interleave_timestamped(&beatmap.timing_points) {
        match smtp_bmtp {
            Ok(soundmap_tp) => {
                last_sound_point = soundmap_tp;

                if let Some(new_tp) = new_timing_points.last_mut() {
                    if soundmap_tp.basically_eq(new_tp) {
                        new_tp.sample_set = soundmap_tp.sample_set;
                        new_tp.sample_index = soundmap_tp.sample_index;
                        new_tp.volume = soundmap_tp.volume;
                    } else {
                        let mut new_tp = new_tp.clone();
                        new_tp.time = soundmap_tp.time;
                        new_tp.uninherited = false;
                        new_tp.sample_set = soundmap_tp.sample_set;
                        new_tp.sample_index = soundmap_tp.sample_index;
                        new_tp.volume = soundmap_tp.volume;
                        new_timing_points.push(new_tp.clone());
                    }
                }
            }
            Err(beatmap_tp) => {
                let mut new_tp = beatmap_tp.clone();
                new_tp.sample_set = last_sound_point.sample_set;
                new_tp.sample_index = last_sound_point.sample_index;
                new_tp.volume = last_sound_point.volume;
                new_timing_points.push(new_tp);
            }
        }
    }
    beatmap.timing_points = new_timing_points;

    tracing::warn!("Inserting soundmap's hitsounds...");
    let slider_multiplier = beatmap.difficulty.as_ref().unwrap().slider_multiplier as f64;

    let mut modified_hit_objects = Vec::new();

    // TODO: improve performance by somehow walking along both maps
    //       (instead of binary-searching the soundmap every time)

    let mut beat_length = 0.0;
    let mut slider_velocity = 1.0;
    for ho_tp in beatmap.iter_hit_objects_and_timing_points() {
        match ho_tp {
            Ok(hit_object) => {
                let new_hit_object = match &hit_object.object_params {
                    HitObjectParams::HitCircle => {
                        // affect hitsound properties of the hitcircle

                        let mut hit_object = hit_object.clone();

                        let start_hitsounds = (soundmap.hit_objects)
                            .between(close_range(hit_object.timestamp(), 2.0));

                        hitsound_hit_object(&mut hit_object, start_hitsounds);
                        hit_object
                    }
                    HitObjectParams::Slider { length, .. } => {
                        // affect all edge hitsound properties of the slider

                        let mut hit_object = hit_object.clone();

                        let start_hitsounds = (soundmap.hit_objects)
                            .between(close_range(hit_object.timestamp(), 2.0));

                        hitsound_hit_object(&mut hit_object, start_hitsounds);

                        let timestamp = hit_object.timestamp();
                        let dur =
                            *length * beat_length / (slider_multiplier * 100.0 * slider_velocity);

                        if let HitObjectParams::Slider {
                            edge_hitsounds,
                            edge_samplesets,
                            ..
                        } = &mut hit_object.object_params
                        {
                            for (i, (edge_hs, edge_ss)) in (edge_hitsounds.iter_mut())
                                .zip(edge_samplesets.iter_mut())
                                .enumerate()
                            {
                                let local_timestamp = timestamp + i as f64 * dur;

                                let start_hitsounds = (soundmap.hit_objects)
                                    .between(close_range(local_timestamp, 2.0));

                                for so in start_hitsounds {
                                    tracing::info!("affecting slider edge at {}", local_timestamp);

                                    if so.hit_sample.normal_set != SampleBank::Auto {
                                        edge_ss.normal_set = so.hit_sample.normal_set;
                                    }

                                    if so.hit_sample.addition_set != SampleBank::Auto {
                                        edge_ss.addition_set = so.hit_sample.addition_set;
                                    }

                                    *edge_hs |= so.hit_sound;
                                }
                            }
                        }

                        hit_object
                    }
                    HitObjectParams::Spinner { end_time } => {
                        // affect hitsound properties of the spinner

                        let mut hit_object = hit_object.clone();

                        let end_hitsounds =
                            (soundmap.hit_objects).between(close_range(*end_time, 2.0));

                        hitsound_hit_object(&mut hit_object, end_hitsounds);
                        hit_object
                    }
                    HitObjectParams::Hold { .. } => {
                        // affect hitsound properties of the mania hold

                        let mut hit_object = hit_object.clone();

                        let start_hitsounds = (soundmap.hit_objects)
                            .between(close_range(hit_object.timestamp(), 2.0));

                        hitsound_hit_object(&mut hit_object, start_hitsounds);
                        hit_object
                    }
                };

                modified_hit_objects.push(new_hit_object);
            }
            Err(timing_point) if timing_point.uninherited => {
                beat_length = timing_point.beat_length;
            }
            Err(timing_point) => {
                slider_velocity = -100.0 / timing_point.beat_length;
            }
        }
    }

    if is_mania {
        tracing::warn!("Applying mania hitsound spread-out transformation...");

        for group in modified_hit_objects.group_timestamped_mut() {
            // Note: due to how the algorithm works, hitobjects in a group all have the same hitsound information.

            match group {
                [] => break,
                [_] => continue,
                [ref mut first, ref mut remains @ ..] => {
                    let normal_set = first.hit_sample.normal_set;
                    let addition_set = first.hit_sample.addition_set;

                    if normal_set != SampleBank::Auto {
                        // Only have the first hitobject on a non-auto normal set
                        for other in remains.iter_mut() {
                            other.hit_sample.normal_set = SampleBank::Auto;
                        }
                    }

                    if addition_set != SampleBank::Auto {
                        // Only have the non-first hitobjects on a non-auto addition set
                        first.hit_sample.addition_set = SampleBank::Auto;
                    }

                    let hit_sound = first.hit_sound;

                    // reset hitsounds for all hitobjects in the group
                    first.hit_sound = HitSound::NONE;
                    for other in remains.iter_mut() {
                        other.hit_sound = HitSound::NONE;
                    }

                    // cycle through remaining hitobjects to give them a separate hitsound each
                    let mut cycle_idx = 0;

                    if hit_sound.has_whistle() {
                        remains[cycle_idx].hit_sound |= HitSound::WHISTLE;
                        cycle_idx = (cycle_idx + 1) % remains.len();
                    }

                    if hit_sound.has_finish() {
                        remains[cycle_idx].hit_sound |= HitSound::FINISH;
                        cycle_idx = (cycle_idx + 1) % remains.len();
                    }

                    if hit_sound.has_clap() {
                        remains[cycle_idx].hit_sound |= HitSound::CLAP;
                    }
                }
            }
        }
    }

    beatmap.hit_objects = modified_hit_objects;

    write_beatmap_out(&beatmap, beatmap_path)?;
    Ok(())
}

fn cli_lazer_to_stable(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut beatmap = parse_beatmap(path, true)?;

    for timing_point in &mut beatmap.timing_points {
        timing_point.time = timing_point.time.floor();
    }

    for hit_object in &mut beatmap.hit_objects {
        hit_object.time = hit_object.time.floor();

        if let HitObjectParams::Slider {
            first_curve_type,
            curve_points,
            ..
        } = &mut hit_object.object_params
        {
            curve_points.insert(
                0,
                SliderPoint {
                    curve_type: *first_curve_type,
                    x: hit_object.x,
                    y: hit_object.y,
                },
            );

            *curve_points = match convert_slider_points_to_legacy(curve_points) {
                Ok(curve_points) => curve_points,
                Err(err) => {
                    tracing::error!("\n{err:?}");
                    return Ok(());
                }
            };

            let first_curve_point = curve_points.remove(0);
            *first_curve_type = first_curve_point.curve_type;
        }
    }

    beatmap.osu_file_format = 14;

    write_beatmap_out(&beatmap, path)?;
    Ok(())
}
