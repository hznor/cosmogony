#[macro_use]
extern crate log;

mod additional_zones;
mod country_finder;
mod hierarchy_builder;
pub mod merger;
mod zone_ext;
pub mod zone_typer;

use crate::country_finder::CountryFinder;
use crate::hierarchy_builder::{build_hierarchy, find_inclusions};
use additional_zones::compute_additional_places;
use anyhow::{anyhow, Context, Error};
use cosmogony::mutable_slice::MutableSlice;
use cosmogony::{Cosmogony, CosmogonyMetadata, CosmogonyStats, ZoneType};
use log::{debug, info};
use osmpbfreader::{OsmId, OsmObj, OsmPbfReader};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use cosmogony::{Zone, ZoneIndex};

use crate::zone_ext::ZoneExt;

const FILE_BUF_SIZE: usize = 1024 * 1024; // 1MB

#[rustfmt::skip]
pub fn is_admin(obj: &OsmObj) -> bool {
    match *obj {
        OsmObj::Relation(ref rel) => {
            rel.tags
                .get("boundary")
                .map_or(false, |v| v == "administrative")
            &&
            rel.tags.get("admin_level").is_some()
        }
        _ => false,
    }
}

pub fn is_place(obj: &OsmObj) -> bool {
    match *obj {
        OsmObj::Node(ref node) => matches!(
            node.tags.get("place").and_then(|s| ZoneType::parse(s)),
            Some(ZoneType::City | ZoneType::Suburb)
        ),
        _ => false,
    }
}

pub fn get_zones_and_stats(
    pbf: &BTreeMap<OsmId, OsmObj>,
) -> Result<(Vec<Zone>, CosmogonyStats), Error> {
    let stats = CosmogonyStats::default();
    let mut zones = Vec::with_capacity(1000);

    for obj in pbf.values() {
        if !is_admin(obj) {
            continue;
        }
        if let OsmObj::Relation(ref relation) = *obj {
            let next_index = ZoneIndex { index: zones.len() };
            if let Some(zone) = Zone::from_osm_relation(relation, pbf, next_index) {
                // Ignore zone without boundary polygon for the moment
                if zone.boundary.is_some() {
                    zones.push(zone);
                }
            };
        }
    }

    Ok((zones, stats))
}

fn get_country_code<'a>(
    country_finder: &'a CountryFinder,
    zone: &Zone,
    country_code: &'a Option<String>,
    inclusions: &[ZoneIndex],
) -> Option<String> {
    if let Some(ref c) = *country_code {
        Some(c.to_uppercase())
    } else {
        country_finder.find_zone_country(zone, inclusions)
    }
}

fn type_zones(
    zones: &mut [Zone],
    stats: &mut CosmogonyStats,
    country_code: Option<String>,
    inclusions: &[Vec<ZoneIndex>],
) -> Result<(), Error> {
    use rayon::prelude::*;
    info!("reading libpostal's rules");
    let zone_typer = zone_typer::ZoneTyper::new()?;

    info!("creating a countries rtree");
    let country_finder: CountryFinder = CountryFinder::init(zones, &zone_typer);
    if country_code.is_none() && country_finder.is_empty() {
        return Err(anyhow!(
            "no country_code has been provided and no country have been found, \
             we won't be able to make a cosmogony",
        ));
    }

    info!("typing zones");
    // We type all the zones in parallele
    // To not mutate the zones while doing it
    // (the borrow checker would not be happy since we also need to access to the zone's vector
    // to be able to transform the ZoneIndex to a zone)
    // we collect all the types in a Vector, and assign the zone's zone_type as a post process
    let zones_type: Vec<_> = zones
        .par_iter()
        .map(|z| {
            get_country_code(&country_finder, z, &country_code, &inclusions[z.id.index]).map(|c| {
                zone_typer
                    .get_zone_type(z, &c, &inclusions[z.id.index], zones)
                    .map(|zone_type| (c, zone_type))
            })
        })
        .collect();

    zones
        .iter_mut()
        .zip(zones_type.into_iter())
        .for_each(
            |(z, country_code_and_zone_type)| match country_code_and_zone_type {
                None => {
                    info!(
                        "impossible to find a country for {} ({}), skipping",
                        z.osm_id, z.name
                    );
                    stats.zone_without_country += 1;
                }
                Some(Ok((country_code, t))) => {
                    z.country_code = Some(country_code);
                    z.zone_type = Some(t)
                }
                Some(Err(zone_typer::ZoneTyperError::InvalidCountry(c))) => {
                    z.country_code = Some(c.clone());
                    info!("impossible to find rules for country {}", c);
                    *stats.zone_with_unkwown_country_rules.entry(c).or_insert(0) += 1;
                }
                Some(Err(zone_typer::ZoneTyperError::UnkownLevel(lvl, country))) => {
                    z.country_code = Some(country.clone());
                    debug!(
                        "impossible to find a rule for level {:?} for country {}",
                        lvl, country
                    );
                    *stats
                        .unhandled_admin_level
                        .entry(country)
                        .or_insert_with(BTreeMap::new)
                        .entry(lvl.unwrap_or(0))
                        .or_insert(0) += 1;
                }
            },
        );

    Ok(())
}

fn compute_labels(zones: &mut [Zone], filter_langs: &[String]) {
    info!("computing all zones's label");
    let nb_zones = zones.len();
    for i in 0..nb_zones {
        let (mslice, z) = MutableSlice::init(zones, i);
        z.compute_labels(&mslice, filter_langs);
    }
}

// we don't want to keep zone's without zone_type (but the zone_type could be ZoneType::NonAdministrative)
fn clean_untagged_zones(zones: &mut Vec<Zone>) {
    info!("cleaning untagged zones");
    let nb_zones = zones.len();
    zones.retain(|z| z.zone_type.is_some());
    info!("{} zones cleaned", (nb_zones - zones.len()));
}

pub fn create_ontology(
    zones: &mut Vec<Zone>,
    stats: &mut CosmogonyStats,
    country_code: Option<String>,
    disable_voronoi: bool,
    parsed_pbf: &BTreeMap<OsmId, OsmObj>,
    filter_langs: &[String],
) -> Result<(), Error> {
    info!("creating ontology for {} zones", zones.len());
    let (inclusions, ztree) = find_inclusions(zones);

    type_zones(zones, stats, country_code, &inclusions)?;

    build_hierarchy(zones, inclusions);

    if !disable_voronoi {
        compute_additional_places(zones, parsed_pbf, ztree);
    }

    zones.iter_mut().for_each(|z| z.compute_names());

    compute_labels(zones, filter_langs);

    // We remove the useless zones from cosmogony.
    //
    // WARNING: this invalidates the different indexes  (we can no longer lookup a Zone by it's id
    // in the zones's vector) this should be removed later on (and switch to a map by osm_id ?) as
    // it's not elegant, but for the moment it'll do.
    clean_untagged_zones(zones);

    Ok(())
}

pub fn build_cosmogony(
    pbf_path: String,
    country_code: Option<String>,
    disable_voronoi: bool,
    filter_langs: &[String],
) -> Result<Cosmogony, Error> {
    let path = Path::new(&pbf_path);
    info!("Reading pbf with geometries...");
    let file = File::open(&path).context("no pbf file")?;
    let file = BufReader::with_capacity(FILE_BUF_SIZE, file);

    let parsed_pbf = OsmPbfReader::new(file)
        .get_objs_and_deps(|o| is_admin(o) || is_place(o))
        .context("invalid osm file")?;
    info!("reading pbf done.");

    let (mut zones, mut stats) = get_zones_and_stats(&parsed_pbf)?;

    create_ontology(
        &mut zones,
        &mut stats,
        country_code,
        disable_voronoi,
        &parsed_pbf,
        filter_langs,
    )?;

    stats.compute(&zones);

    let cosmogony = Cosmogony {
        zones,
        meta: CosmogonyMetadata {
            osm_filename: path
                .file_name()
                .and_then(|f| f.to_str())
                .map(|f| f.to_string())
                .unwrap_or_else(|| "invalid file name".into()),
            stats,
        },
    };
    Ok(cosmogony)
}
