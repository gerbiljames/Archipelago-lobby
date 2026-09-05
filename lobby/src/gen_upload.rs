use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::{BufReader, Read, Seek},
    path::Path,
};

use anyhow::anyhow;
use counter::Counter;
use diesel_async::AsyncPgConnection;
use once_cell::sync::Lazy;
use regex::Regex;
use rocket::{form::FromForm, fs::TempFile, http::Status};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use wq::JobId;
use zip::ZipArchive;

use crate::{
    db::{self, GenerationStatus, RoomId, YamlId, YamlWithoutContent},
    error::{ApiError, ApiResult},
    generation::get_slots,
    yaml::get_ap_player_name,
};

/// Matches AP's `get_out_file_name_base`: `AP_<seed>_P<slot>_<file safe player name>.<ext>`.
static AP_PATCH_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^AP[_-]([0-9]+)[_-]P([0-9]+)[_-](.*)\.[^.]*$").unwrap());

const MULTIDATA_MAX_FORMAT_VERSION: u8 = 3;

#[derive(FromForm)]
pub struct GenerationUploadForm<'r> {
    pub seed: TempFile<'r>,
    pub passwords: Option<TempFile<'r>>,
}

/// One entry of the `AP_<seed>_slot_passwords.json` written next to the seed zip.
#[derive(Deserialize, Debug, Clone)]
pub struct SlotPassword {
    pub slot: usize,
    pub name: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedSlot {
    pub slot: usize,
    pub yaml_id: YamlId,
    pub name: String,
    pub game: String,
}

/// The slots AP would assign to this room's YAMLs, in AP slot order, with the
/// connect name AP derives from each YAML's `name`.
pub fn expected_slots(room_yamls: &[YamlWithoutContent]) -> Vec<ExpectedSlot> {
    let by_id: HashMap<YamlId, &YamlWithoutContent> =
        room_yamls.iter().map(|yaml| (yaml.id, yaml)).collect();
    let mut counter: Counter<String> = Counter::new();

    get_slots(room_yamls)
        .into_iter()
        .enumerate()
        .map(|(i, (_, yaml_id))| {
            let yaml = by_id[&yaml_id];
            ExpectedSlot {
                slot: i + 1,
                yaml_id,
                name: get_ap_player_name(&yaml.player_name, &mut counter),
                game: yaml.game.clone(),
            }
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct ValidatedGeneration {
    pub seed_name: String,
    pub patches: HashMap<YamlId, String>,
    /// `None` when no passwords file was supplied: existing passwords are left untouched.
    pub passwords: Option<Vec<(YamlId, String)>>,
    /// Mismatches that don't block the upload, e.g. a player whose YAML
    /// triggers rename them at generation time.
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct GenerationMismatch(pub Vec<String>);

impl fmt::Display for GenerationMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.join("\n"))
    }
}

impl std::error::Error for GenerationMismatch {}

#[derive(Deserialize)]
struct PatchManifest {
    player: Option<usize>,
    player_name: Option<String>,
    game: Option<String>,
}

fn read_patch_manifest<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Option<PatchManifest> {
    let mut bytes = Vec::new();
    archive.by_name(name).ok()?.read_to_end(&mut bytes).ok()?;
    let mut container = ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    let mut manifest = String::new();
    container
        .by_name("archipelago.json")
        .ok()?
        .read_to_string(&mut manifest)
        .ok()?;
    serde_json::from_str(&manifest).ok()
}

fn file_safe_name(name: &str) -> String {
    name.chars()
        .filter(|c| !"<>:\"/\\|?*".contains(*c))
        .collect::<String>()
        .replace(' ', "_")
}

fn game_matches(expected: &str, actual: &str) -> bool {
    expected == actual || expected.starts_with("Random (")
}

/// A world can emit several files for one slot; the one served as the slot's
/// patch is the first AP container, or failing that the first file.
fn should_replace_patch(current_is_container: Option<bool>, new_is_container: bool) -> bool {
    match current_is_container {
        None => true,
        Some(true) => false,
        Some(false) => new_is_container,
    }
}

/// Per-slot output files of a generation zip, keyed by slot number, using the
/// same file naming and preference rules as [`validate_generation`].
pub fn slot_outputs<R: Read + Seek>(archive: &mut ZipArchive<R>) -> HashMap<usize, String> {
    let names: Vec<String> = archive
        .file_names()
        .filter(|name| !name.ends_with('/'))
        .map(str::to_string)
        .collect();
    let mut chosen: HashMap<usize, (String, bool)> = HashMap::new();
    for name in names {
        let Some(caps) = AP_PATCH_RE.captures(&name) else {
            continue;
        };
        let Ok(slot) = caps[2].parse::<usize>() else {
            continue;
        };
        let is_container = read_patch_manifest(archive, &name).is_some();
        if should_replace_patch(chosen.get(&slot).map(|(_, c)| *c), is_container) {
            chosen.insert(slot, (name, is_container));
        }
    }
    chosen
        .into_iter()
        .map(|(slot, (name, _))| (slot, name))
        .collect()
}

/// Checks that a generation output zip (and its optional slot passwords) was
/// generated from this room's YAMLs, and returns what to associate with each YAML.
pub fn validate_generation<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    passwords: Option<&[SlotPassword]>,
    expected: &[ExpectedSlot],
) -> Result<ValidatedGeneration, GenerationMismatch> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let names: Vec<String> = archive
        .file_names()
        .filter(|name| !name.ends_with('/'))
        .map(str::to_string)
        .collect();
    let slot_count = expected.len();

    let multidata: Vec<&String> = names
        .iter()
        .filter(|name| name.ends_with(".archipelago"))
        .collect();
    let seed_name = match multidata.as_slice() {
        [name] => match name
            .strip_prefix("AP_")
            .and_then(|n| n.strip_suffix(".archipelago"))
        {
            Some(seed) => Some(seed.to_string()),
            None => {
                errors.push(format!(
                    "Unexpected multidata file name {name:?}, expected AP_<seed>.archipelago"
                ));
                None
            }
        },
        [] => {
            errors.push("The zip doesn't contain a .archipelago multidata file".to_string());
            None
        }
        _ => {
            errors.push("The zip contains more than one .archipelago multidata file".to_string());
            None
        }
    };

    if let [name] = multidata.as_slice() {
        let mut version = [0u8; 1];
        match archive
            .by_name(name)
            .map(|mut f| f.read_exact(&mut version))
        {
            Ok(Ok(())) if version[0] <= MULTIDATA_MAX_FORMAT_VERSION => {}
            Ok(Ok(())) => errors.push(format!(
                "Unsupported multidata format version {}",
                version[0]
            )),
            _ => errors.push(format!("Couldn't read the multidata file {name:?}")),
        }
    }

    let mut patches: HashMap<YamlId, String> = HashMap::new();
    let mut slot_is_container: HashMap<usize, bool> = HashMap::new();
    for name in &names {
        let Some(caps) = AP_PATCH_RE.captures(name) else {
            continue;
        };
        if let Some(seed) = &seed_name {
            if &caps[1] != seed {
                errors.push(format!(
                    "{name} belongs to seed {} but the multidata is for seed {seed}",
                    &caps[1]
                ));
                continue;
            }
        }
        let slot: usize = caps[2].parse().unwrap_or(0);
        let Some(exp) = slot.checked_sub(1).and_then(|i| expected.get(i)) else {
            errors.push(format!(
                "{name} is for slot {slot} but this room only has {slot_count} slots"
            ));
            continue;
        };

        let manifest = read_patch_manifest(archive, name);
        if let Some(manifest) = &manifest {
            if let Some(player) = manifest.player {
                if player != slot {
                    errors.push(format!(
                        "{name} is named for slot {slot} but its manifest says it's for slot {player}"
                    ));
                }
            }
            if let Some(player_name) = &manifest.player_name {
                if player_name != &exp.name {
                    warnings.push(format!(
                        "{name} is for player {player_name:?} but slot {slot} in this room is {:?}",
                        exp.name
                    ));
                }
            }
            if let Some(game) = &manifest.game {
                if !game_matches(&exp.game, game) {
                    errors.push(format!(
                        "{name} is a {game:?} output but slot {slot} ({}) in this room plays {:?}",
                        exp.name, exp.game
                    ));
                }
            }
        } else if !caps[3].starts_with(&file_safe_name(&exp.name)) {
            warnings.push(format!(
                "{name} doesn't look like an output for slot {slot} ({}) in this room",
                exp.name
            ));
        }

        let is_container = manifest.is_some();
        if should_replace_patch(slot_is_container.get(&slot).copied(), is_container) {
            slot_is_container.insert(slot, is_container);
            patches.insert(exp.yaml_id, name.clone());
        }
    }

    let mut password_updates = passwords.map(|_| Vec::new());
    if let (Some(passwords), Some(password_updates)) = (passwords, password_updates.as_mut()) {
        let mut seen = HashSet::new();
        for entry in passwords {
            let Some(exp) = entry.slot.checked_sub(1).and_then(|i| expected.get(i)) else {
                errors.push(format!(
                    "The passwords file lists slot {} ({:?}) but this room only has {slot_count} slots",
                    entry.slot, entry.name
                ));
                continue;
            };
            if !seen.insert(entry.slot) {
                errors.push(format!(
                    "The passwords file lists slot {} more than once",
                    entry.slot
                ));
                continue;
            }
            if entry.name != exp.name {
                warnings.push(format!(
                    "The passwords file says slot {} is {:?} but slot {} in this room is {:?}",
                    entry.slot, entry.name, exp.slot, exp.name
                ));
            }
            password_updates.push((exp.yaml_id, entry.password.clone()));
        }
        let missing: Vec<String> = expected
            .iter()
            .filter(|exp| !seen.contains(&exp.slot))
            .map(|exp| format!("{} ({})", exp.slot, exp.name))
            .collect();
        if !missing.is_empty() {
            errors.push(format!(
                "The passwords file has no password for slot(s) {}",
                missing.join(", ")
            ));
        }
    }

    if !errors.is_empty() {
        return Err(GenerationMismatch(errors));
    }

    Ok(ValidatedGeneration {
        seed_name: seed_name.unwrap_or_default(),
        patches,
        passwords: password_updates,
        warnings,
    })
}

#[derive(Serialize)]
pub struct UploadGenerationResult {
    pub job_id: String,
    pub seed_name: String,
    pub patches_associated: usize,
    pub passwords_set: usize,
    pub warnings: Vec<String>,
}

/// Older generators wrote the slot passwords inside the zip rather than next to it.
fn bundled_passwords<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> ApiResult<Option<Vec<SlotPassword>>> {
    let Some(name) = archive
        .file_names()
        .find(|n| n.ends_with("_slot_passwords.json"))
        .map(str::to_string)
    else {
        return Ok(None);
    };
    let mut contents = String::new();
    archive.by_name(&name)?.read_to_string(&mut contents)?;
    serde_json::from_str(&contents)
        .map(Some)
        .map_err(|e| bad_request(format!("Couldn't parse {name} from the zip: {e}")))
}

fn bad_request(message: String) -> ApiError {
    ApiError {
        error: anyhow!(message),
        status: Status::BadRequest,
    }
}

async fn remove_upload_dir(job_dir: &Path) {
    if let Err(e) = tokio::fs::remove_dir_all(job_dir).await {
        tracing::warn!(error = %e, dir = %job_dir.display(), "Failed to remove upload dir");
    }
}

/// What the generation page shows in place of a worker's log.
fn upload_log(validated: &ValidatedGeneration, expected: &[ExpectedSlot]) -> String {
    let mut log = format!("Uploaded generation for seed {}\n\n", validated.seed_name);
    if !validated.warnings.is_empty() {
        log.push_str("Warnings:\n");
        for warning in &validated.warnings {
            log.push_str(&format!("  {warning}\n"));
        }
        log.push('\n');
    }
    for exp in expected {
        let patch = validated
            .patches
            .get(&exp.yaml_id)
            .map(String::as_str)
            .unwrap_or("no output file");
        let password = match &validated.passwords {
            Some(passwords) if passwords.iter().any(|(id, _)| *id == exp.yaml_id) => {
                ", password set"
            }
            Some(_) => ", no password",
            None => "",
        };
        log.push_str(&format!(
            "Slot {}: {} ({}): {patch}{password}\n",
            exp.slot, exp.name, exp.game
        ));
    }
    if validated.passwords.is_none() {
        log.push_str("\nNo passwords file uploaded, existing slot passwords were left unchanged\n");
    }
    log
}

/// Stores an uploaded generation for a room after checking it matches the
/// room's slots. The caller is responsible for authorization.
#[tracing::instrument(skip(form, gen_output_dir, conn))]
pub async fn ingest_generation_upload(
    room_id: RoomId,
    mut form: GenerationUploadForm<'_>,
    gen_output_dir: &Path,
    conn: &mut AsyncPgConnection,
) -> ApiResult<UploadGenerationResult> {
    if form.seed.is_empty() {
        return Err(bad_request("No seed zip was uploaded".to_string()));
    }

    let passwords: Option<Vec<SlotPassword>> =
        match form.passwords.as_mut().filter(|f| !f.is_empty()) {
            Some(file) => {
                let mut contents = String::new();
                file.open().await?.read_to_string(&mut contents).await?;
                Some(serde_json::from_str(&contents).map_err(|e| {
                    bad_request(format!("Couldn't parse the slot passwords file: {e}"))
                })?)
            }
            None => None,
        };

    let previous = db::get_generation_for_room(room_id, conn).await?;
    if let Some(previous) = &previous {
        if matches!(
            previous.status,
            GenerationStatus::Pending | GenerationStatus::Running
        ) {
            return Err(ApiError {
                error: anyhow!(
                    "A generation is in progress for this room, cancel it before uploading one"
                ),
                status: Status::Conflict,
            });
        }
    }

    let room_yamls: Vec<_> = db::get_yamls_for_room_with_author_names(room_id, conn)
        .await?
        .into_iter()
        .map(|(yaml, _)| yaml)
        .collect();
    let expected = expected_slots(&room_yamls);

    let job_id = JobId::new();
    let job_dir = gen_output_dir.join(job_id.to_string());
    tokio::fs::create_dir_all(&job_dir).await?;
    let zip_path = job_dir.join("AP_upload.zip");

    let validated = async {
        form.seed.move_copy_to(&zip_path).await?;
        let zip_path = zip_path.clone();
        let expected = expected.clone();
        tokio::task::spawn_blocking(move || -> ApiResult<ValidatedGeneration> {
            let file = std::fs::File::open(&zip_path)?;
            let mut archive = ZipArchive::new(BufReader::new(file))
                .map_err(|e| bad_request(format!("Uploaded file is not a valid zip: {e}")))?;
            let passwords = match passwords {
                Some(passwords) => Some(passwords),
                None => bundled_passwords(&mut archive)?,
            };
            validate_generation(&mut archive, passwords.as_deref(), &expected)
                .map_err(|e| bad_request(format!("This generation doesn't match the room:\n{e}")))
        })
        .await
        .map_err(|e| anyhow!("Validation task panicked: {e}"))?
    }
    .await;

    let validated = match validated {
        Ok(validated) => validated,
        Err(e) => {
            remove_upload_dir(&job_dir).await;
            return Err(e);
        }
    };

    let patches_associated = validated.patches.len();
    let passwords_set = validated.passwords.as_ref().map_or(0, Vec::len);

    let log = upload_log(&validated, &expected);
    if let Err(e) = tokio::fs::write(job_dir.join("upload.log"), log).await {
        tracing::warn!(error = %e, dir = %job_dir.display(), "Failed to write upload log");
    }

    let published = db::publish_generation(
        room_id,
        job_id,
        validated.patches,
        validated.passwords,
        previous.as_ref().map(|p| p.job_id),
        conn,
    )
    .await;
    match published {
        Ok(true) => {}
        Ok(false) => {
            remove_upload_dir(&job_dir).await;
            return Err(ApiError {
                error: anyhow!(
                    "This room's generation changed while the upload was being processed, try again"
                ),
                status: Status::Conflict,
            });
        }
        Err(e) => {
            remove_upload_dir(&job_dir).await;
            return Err(e.into());
        }
    }

    if let Some(previous) = previous.filter(|p| p.job_id != job_id) {
        let old_dir = gen_output_dir.join(previous.job_id.to_string());
        if let Err(e) = tokio::fs::remove_dir_all(&old_dir).await {
            tracing::warn!(error = %e, dir = %old_dir.display(), "Failed to remove replaced generation dir");
        }
    }

    Ok(UploadGenerationResult {
        job_id: job_id.to_string(),
        seed_name: validated.seed_name,
        patches_associated,
        passwords_set,
        warnings: validated.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn slot(n: usize, name: &str, game: &str) -> ExpectedSlot {
        ExpectedSlot {
            slot: n,
            yaml_id: YamlId::new_v4(),
            name: name.to_string(),
            game: game.to_string(),
        }
    }

    fn container(player: usize, player_name: &str, game: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("archipelago.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(
            serde_json::json!({
                "compatible_version": 5, "version": 7, "server": "",
                "player": player, "player_name": player_name, "game": game,
                "patch_file_ending": ".aptest",
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        zip.start_file("patch.aptest", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"payload").unwrap();
        zip.finish().unwrap().into_inner()
    }

    fn seed_zip(entries: &[(&str, &[u8])]) -> ZipArchive<Cursor<Vec<u8>>> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(content).unwrap();
        }
        ZipArchive::new(zip.finish().unwrap()).unwrap()
    }

    const MULTIDATA: &[u8] = &[3, 0x78, 0x9c];

    fn good_seed() -> ZipArchive<Cursor<Vec<u8>>> {
        seed_zip(&[
            ("AP_123.archipelago", MULTIDATA),
            ("AP_123_Spoiler.txt", b"spoiler"),
            ("AP_123_P1_Alice.aptest", &container(1, "Alice", "Game A")),
            ("AP_123_P2_Bob.aptest", &container(2, "Bob", "Game B")),
        ])
    }

    fn room() -> Vec<ExpectedSlot> {
        vec![slot(1, "Alice", "Game A"), slot(2, "Bob", "Game B")]
    }

    fn errors_of<T>(result: Result<T, GenerationMismatch>) -> Vec<String> {
        match result {
            Ok(_) => panic!("expected a mismatch"),
            Err(e) => e.0,
        }
    }

    #[test]
    fn accepts_matching_generation() {
        let expected = room();
        let passwords = [
            SlotPassword {
                slot: 1,
                name: "Alice".into(),
                password: "aaaa".into(),
            },
            SlotPassword {
                slot: 2,
                name: "Bob".into(),
                password: "bbbb".into(),
            },
        ];
        let validated = validate_generation(&mut good_seed(), Some(&passwords), &expected).unwrap();

        assert_eq!(validated.seed_name, "123");
        assert_eq!(validated.patches.len(), 2);
        assert_eq!(
            validated.patches[&expected[0].yaml_id],
            "AP_123_P1_Alice.aptest"
        );
        assert_eq!(
            validated.patches[&expected[1].yaml_id],
            "AP_123_P2_Bob.aptest"
        );
        assert_eq!(
            validated.passwords,
            Some(vec![
                (expected[0].yaml_id, "aaaa".to_string()),
                (expected[1].yaml_id, "bbbb".to_string())
            ])
        );
    }

    #[test]
    fn accepts_generation_without_passwords_or_patches_for_every_slot() {
        let expected = vec![
            slot(1, "Alice", "Game A"),
            slot(2, "Bob", "Game B"),
            slot(3, "Carol", "Game C"),
        ];
        let validated = validate_generation(&mut good_seed(), None, &expected).unwrap();
        assert_eq!(validated.patches.len(), 2);
        assert!(validated.passwords.is_none());
    }

    #[test]
    fn rejects_missing_multidata() {
        let mut zip = seed_zip(&[("AP_123_P1_Alice.aptest", &container(1, "Alice", "Game A"))]);
        let errors = errors_of(validate_generation(&mut zip, None, &room()));
        assert!(
            errors.iter().any(|e| e.contains(".archipelago")),
            "{errors:?}"
        );
    }

    #[test]
    fn rejects_unsupported_multidata_version() {
        let mut zip = seed_zip(&[
            ("AP_123.archipelago", &[9, 0, 0]),
            ("AP_123_P1_Alice.aptest", &container(1, "Alice", "Game A")),
        ]);
        let errors = errors_of(validate_generation(&mut zip, None, &room()));
        assert!(
            errors.iter().any(|e| e.contains("format version 9")),
            "{errors:?}"
        );
    }

    #[test]
    fn warns_about_wrong_player_names() {
        let expected = vec![slot(1, "Alice", "Game A"), slot(2, "Robert", "Game B")];
        let validated = validate_generation(&mut good_seed(), None, &expected).unwrap();
        assert_eq!(validated.patches.len(), 2);
        assert_eq!(validated.warnings.len(), 1, "{:?}", validated.warnings);
        assert!(
            validated.warnings[0].contains("\"Bob\"")
                && validated.warnings[0].contains("\"Robert\""),
            "{:?}",
            validated.warnings
        );
    }

    #[test]
    fn rejects_wrong_games_but_allows_random_yamls() {
        let expected = vec![slot(1, "Alice", "Game X"), slot(2, "Bob", "Random (3)")];
        let errors = errors_of(validate_generation(&mut good_seed(), None, &expected));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("AP_123_P1_Alice.aptest"), "{errors:?}");
    }

    #[test]
    fn rejects_more_slots_than_the_room_has() {
        let expected = vec![slot(1, "Alice", "Game A")];
        let errors = errors_of(validate_generation(&mut good_seed(), None, &expected));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("slot 2") && e.contains("only has 1 slots")),
            "{errors:?}"
        );
    }

    #[test]
    fn rejects_patches_from_another_seed() {
        let mut zip = seed_zip(&[
            ("AP_123.archipelago", MULTIDATA),
            ("AP_123_P1_Alice.aptest", &container(1, "Alice", "Game A")),
            ("AP_999_P2_Bob.aptest", &container(2, "Bob", "Game B")),
        ]);
        let errors = errors_of(validate_generation(&mut zip, None, &room()));
        assert!(errors.iter().any(|e| e.contains("seed 999")), "{errors:?}");
    }

    #[test]
    fn rejects_passwords_that_dont_match_the_room() {
        let passwords = [
            SlotPassword {
                slot: 1,
                name: "Alice".into(),
                password: "aaaa".into(),
            },
            SlotPassword {
                slot: 2,
                name: "Eve".into(),
                password: "eeee".into(),
            },
            SlotPassword {
                slot: 3,
                name: "Mallory".into(),
                password: "mmmm".into(),
            },
        ];
        let errors = errors_of(validate_generation(
            &mut good_seed(),
            Some(&passwords),
            &room(),
        ));
        assert!(errors.iter().any(|e| e.contains("slot 3")), "{errors:?}");
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn warns_about_password_names_but_still_sets_them() {
        let passwords = [
            SlotPassword {
                slot: 1,
                name: "Alice".into(),
                password: "aaaa".into(),
            },
            SlotPassword {
                slot: 2,
                name: "Eve".into(),
                password: "eeee".into(),
            },
        ];
        let expected = room();
        let validated = validate_generation(&mut good_seed(), Some(&passwords), &expected).unwrap();
        assert_eq!(validated.warnings.len(), 1, "{:?}", validated.warnings);
        assert!(
            validated.warnings[0].contains("\"Eve\"") && validated.warnings[0].contains("\"Bob\""),
            "{:?}",
            validated.warnings
        );
        assert_eq!(
            validated.passwords,
            Some(vec![
                (expected[0].yaml_id, "aaaa".to_string()),
                (expected[1].yaml_id, "eeee".to_string())
            ])
        );
    }

    #[test]
    fn rejects_passwords_that_miss_a_slot() {
        let passwords = [SlotPassword {
            slot: 1,
            name: "Alice".into(),
            password: "aaaa".into(),
        }];
        let errors = errors_of(validate_generation(
            &mut good_seed(),
            Some(&passwords),
            &room(),
        ));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("no password for slot(s) 2 (Bob)"),
            "{errors:?}"
        );
    }

    #[test]
    fn falls_back_to_file_name_for_non_container_outputs() {
        let mut zip = seed_zip(&[
            ("AP_123.archipelago", MULTIDATA),
            ("AP_123_P1_Alice.bin", b"raw"),
            ("AP_123_P2_Bob.bin", b"raw"),
        ]);
        let expected = vec![slot(1, "Alice", "Game A"), slot(2, "Robert", "Game B")];
        let validated = validate_generation(&mut zip, None, &expected).unwrap();
        assert_eq!(validated.patches.len(), 2);
        assert_eq!(validated.warnings.len(), 1, "{:?}", validated.warnings);
        assert!(
            validated.warnings[0].contains("AP_123_P2_Bob.bin"),
            "{:?}",
            validated.warnings
        );
    }

    #[test]
    fn prefers_the_container_when_a_slot_has_several_outputs() {
        let mut zip = seed_zip(&[
            ("AP_123.archipelago", MULTIDATA),
            ("AP_123_P1_Alice.txt", b"notes"),
            ("AP_123_P1_Alice.aptest", &container(1, "Alice", "Game A")),
            ("AP_123_P1_Alice.bin", b"raw"),
        ]);
        let expected = vec![slot(1, "Alice", "Game A")];
        let validated = validate_generation(&mut zip, None, &expected).unwrap();
        assert_eq!(
            validated.patches[&expected[0].yaml_id],
            "AP_123_P1_Alice.aptest"
        );
    }

    #[test]
    fn accepts_generation_whose_games_emit_no_output_files() {
        let mut zip = seed_zip(&[
            ("AP_123.archipelago", MULTIDATA),
            ("AP_123_Spoiler.txt", b""),
        ]);
        let validated = validate_generation(&mut zip, None, &room()).unwrap();
        assert_eq!(validated.seed_name, "123");
        assert!(validated.patches.is_empty());
        assert!(validated.passwords.is_none());
    }

    #[test]
    fn ignores_directory_entries() {
        let mut zip = seed_zip(&[
            ("AP_123.archipelago", MULTIDATA),
            ("AP_123_P1_Alice.x/", b""),
            ("AP_123_P2_Bob.aptest", &container(2, "Bob", "Game B")),
        ]);
        let expected = room();
        let validated = validate_generation(&mut zip, None, &expected).unwrap();
        assert_eq!(validated.patches.len(), 1);
        assert!(!validated.patches.contains_key(&expected[0].yaml_id));
        assert_eq!(slot_outputs(&mut zip).len(), 1);
    }

    #[test]
    fn slot_outputs_prefers_containers_like_validation() {
        let mut zip = seed_zip(&[
            ("AP_123.archipelago", MULTIDATA),
            ("AP_123_P1_Alice.txt", b"notes"),
            ("AP_123_P1_Alice.aptest", &container(1, "Alice", "Game A")),
            ("AP_123_P1_Alice.bin", b"raw"),
            ("AP_123_P2_Bob.bin", b"raw"),
            ("AP_123_P0_Nobody.bin", b"raw"),
        ]);
        let outputs = slot_outputs(&mut zip);
        assert_eq!(outputs[&1], "AP_123_P1_Alice.aptest");
        assert_eq!(outputs[&2], "AP_123_P2_Bob.bin");
        assert_eq!(outputs.len(), 3);
    }

    #[test]
    fn expected_slots_follow_ap_ordering_and_naming() {
        use crate::db::{BundleId, Json, YamlValidationStatus};
        use crate::extractor::YamlFeatures;
        use chrono::NaiveDateTime;

        let yaml = |name: &str, game: &str| YamlWithoutContent {
            id: YamlId::new_v4(),
            player_name: name.to_string(),
            game: game.to_string(),
            owner_id: 1,
            features: Json(YamlFeatures::default()),
            validation_status: YamlValidationStatus::Validated,
            patch: None,
            bundle_id: BundleId::new_v4(),
            password: None,
            created_at: NaiveDateTime::default(),
            last_edited_by_name: None,
        };
        let yamls = vec![
            yaml("zed", "Game Z"),
            yaml("Player{number}", "Game A"),
            yaml("Player{number}", "Game B"),
            yaml("A very long player name indeed", "Game L"),
        ];

        let slots = expected_slots(&yamls);
        let summary: Vec<(usize, &str, &str)> = slots
            .iter()
            .map(|s| (s.slot, s.name.as_str(), s.game.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (1, "A very long play", "Game L"),
                (2, "Player1", "Game A"),
                (3, "Player2", "Game B"),
                (4, "zed", "Game Z"),
            ]
        );
        assert_eq!(slots[3].yaml_id, yamls[0].id);
    }
}
