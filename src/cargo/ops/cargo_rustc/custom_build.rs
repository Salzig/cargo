use std::collections::HashMap;
use std::fs::{self, File};
use std::io::prelude::*;
use std::path::PathBuf;
use std::str;
use std::sync::Mutex;

use core::{Package, Target, PackageId, PackageSet, Profile};
use util::{CargoResult, human, Human};
use util::{internal, ChainError, profile};
use util::Freshness;

use super::job::Work;
use super::{fingerprint, process, Kind, Context, Platform};
use super::CommandType;
use super::PackagesToBuild;

/// Contains the parsed output of a custom build script.
#[derive(Clone, Debug)]
pub struct BuildOutput {
    /// Paths to pass to rustc with the `-L` flag
    pub library_paths: Vec<PathBuf>,
    /// Names and link kinds of libraries, suitable for the `-l` flag
    pub library_links: Vec<String>,
    /// Various `--cfg` flags to pass to the compiler
    pub cfgs: Vec<String>,
    /// Metadata to pass to the immediate dependencies
    pub metadata: Vec<(String, String)>,
}

pub type BuildMap = HashMap<(PackageId, Kind), BuildOutput>;

pub struct BuildState {
    pub outputs: Mutex<BuildMap>,
}

/// Prepares a `Work` that executes the target as a custom build script.
///
/// The `req` given is the requirement which this run of the build script will
/// prepare work for. If the requirement is specified as both the target and the
/// host platforms it is assumed that the two are equal and the build script is
/// only run once (not twice).
pub fn prepare(pkg: &Package, target: &Target, req: Platform,
               cx: &mut Context) -> CargoResult<(Work, Work, Freshness)> {
    let _p = profile::start(format!("build script prepare: {}/{}",
                                    pkg, target.name()));
    let kind = match req { Platform::Plugin => Kind::Host, _ => Kind::Target, };
    let (script_output, build_output) = {
        (cx.layout(pkg, Kind::Host).build(pkg),
         cx.layout(pkg, kind).build_out(pkg))
    };

    // Building the command to execute
    let to_exec = script_output.join(target.name());

    // Start preparing the process to execute, starting out with some
    // environment variables. Note that the profile-related environment
    // variables are not set with this the build script's profile but rather the
    // package's library profile.
    let profile = cx.lib_profile(pkg.package_id());
    let to_exec = to_exec.into_os_string();
    let mut p = try!(super::process(CommandType::Host(to_exec), pkg, target, cx));
    p.env("OUT_DIR", &build_output)
     .env("CARGO_MANIFEST_DIR", pkg.root())
     .env("NUM_JOBS", &cx.jobs().to_string())
     .env("TARGET", &match kind {
         Kind::Host => &cx.config.rustc_info().host[..],
         Kind::Target => cx.target_triple(),
     })
     .env("DEBUG", &profile.debuginfo.to_string())
     .env("OPT_LEVEL", &profile.opt_level.to_string())
     .env("PROFILE", if cx.build_config.release {"release"} else {"debug"})
     .env("HOST", &cx.config.rustc_info().host);

    // Be sure to pass along all enabled features for this package, this is the
    // last piece of statically known information that we have.
    match cx.resolve.features(pkg.package_id()) {
        Some(features) => {
            for feat in features.iter() {
                p.env(&format!("CARGO_FEATURE_{}", super::envify(feat)), "1");
            }
        }
        None => {}
    }

    // Gather the set of native dependencies that this package has along with
    // some other variables to close over.
    //
    // This information will be used at build-time later on to figure out which
    // sorts of variables need to be discovered at that time.
    let lib_deps = {
        let not_custom = pkg.targets().iter().find(|t| {
            !t.is_custom_build()
        }).unwrap();
        cx.dep_targets(pkg, not_custom, kind, profile).iter()
                                                      .filter_map(|&(pkg, t, _)| {
            if !t.linkable() { return None }
            pkg.manifest().links().map(|links| {
                (links.to_string(), pkg.package_id().clone())
            })
        }).collect::<Vec<_>>()
    };
    let pkg_name = pkg.to_string();
    let build_state = cx.build_state.clone();
    let id = pkg.package_id().clone();
    let all = (id.clone(), pkg_name.clone(), build_state.clone(),
               build_output.clone());
    let plugin_deps = super::load_build_deps(cx, pkg, target, profile,
                                             Kind::Host);

    try!(fs::create_dir_all(&cx.layout(pkg, Kind::Target).build(pkg)));
    try!(fs::create_dir_all(&cx.layout(pkg, Kind::Host).build(pkg)));

    let exec_engine = cx.exec_engine.clone();

    // Prepare the unit of "dirty work" which will actually run the custom build
    // command.
    //
    // Note that this has to do some extra work just before running the command
    // to determine extra environment variables and such.
    let work = Work::new(move |desc_tx| {
        // Make sure that OUT_DIR exists.
        //
        // If we have an old build directory, then just move it into place,
        // otherwise create it!
        if fs::metadata(&build_output).is_err() {
            try!(fs::create_dir(&build_output).chain_error(|| {
                internal("failed to create script output directory for \
                          build command")
            }));
        }

        // For all our native lib dependencies, pick up their metadata to pass
        // along to this custom build command. We're also careful to augment our
        // dynamic library search path in case the build script depended on any
        // native dynamic libraries.
        {
            let build_state = build_state.outputs.lock().unwrap();
            for &(ref name, ref id) in lib_deps.iter() {
                let data = &build_state[&(id.clone(), kind)].metadata;
                for &(ref key, ref value) in data.iter() {
                    p.env(&format!("DEP_{}_{}", super::envify(name),
                                   super::envify(key)), value);
                }
            }
            try!(super::add_plugin_deps(&mut p, &build_state, plugin_deps));
        }

        // And now finally, run the build command itself!
        desc_tx.send(p.to_string()).ok();
        let output = try!(exec_engine.exec_with_output(p).map_err(|mut e| {
            e.desc = format!("failed to run custom build command for `{}`\n{}",
                             pkg_name, e.desc);
            Human(e)
        }));

        // After the build command has finished running, we need to be sure to
        // remember all of its output so we can later discover precisely what it
        // was, even if we don't run the build command again (due to freshness).
        //
        // This is also the location where we provide feedback into the build
        // state informing what variables were discovered via our script as
        // well.
        let output = try!(str::from_utf8(&output.stdout).map_err(|_| {
            human("build script output was not valid utf-8")
        }));
        let parsed_output = try!(BuildOutput::parse(output, &pkg_name));
        build_state.insert(id, req, parsed_output);

        try!(File::create(&build_output.parent().unwrap().join("output"))
                  .and_then(|mut f| f.write_all(output.as_bytes()))
                  .map_err(|e| {
            human(format!("failed to write output of custom build command: {}",
                          e))
        }));

        Ok(())
    });

    // Now that we've prepared our work-to-do, we need to prepare the fresh work
    // itself to run when we actually end up just discarding what we calculated
    // above.
    //
    // Note that the freshness calculation here is the build_cmd freshness, not
    // target specific freshness. This is because we don't actually know what
    // the inputs are to this command!
    //
    // Also note that a fresh build command needs to
    let (freshness, dirty, fresh) =
            try!(fingerprint::prepare_build_cmd(cx, pkg, kind));
    let dirty = Work::new(move |tx| {
        try!(work.call((tx.clone())));
        dirty.call(tx)
    });
    let fresh = Work::new(move |tx| {
        let (id, pkg_name, build_state, build_output) = all;
        let new_loc = build_output.parent().unwrap().join("output");
        let mut f = try!(File::open(&new_loc).map_err(|e| {
            human(format!("failed to read cached build command output: {}", e))
        }));
        let mut contents = String::new();
        try!(f.read_to_string(&mut contents));
        let output = try!(BuildOutput::parse(&contents, &pkg_name));
        build_state.insert(id, req, output);

        fresh.call(tx)
    });

    Ok((dirty, fresh, freshness))
}

impl BuildState {
    pub fn new(config: &super::BuildConfig,
               packages: &PackageSet) -> BuildState {
        let mut sources = HashMap::new();
        for package in packages.iter() {
            match package.manifest().links() {
                Some(links) => {
                    sources.insert(links.to_string(),
                                   package.package_id().clone());
                }
                None => {}
            }
        }
        let mut outputs = HashMap::new();
        let i1 = config.host.overrides.iter().map(|p| (p, Kind::Host));
        let i2 = config.target.overrides.iter().map(|p| (p, Kind::Target));
        for ((name, output), kind) in i1.chain(i2) {
            // If no package is using the library named `name`, then this is
            // just an override that we ignore.
            if let Some(id) = sources.get(name) {
                outputs.insert((id.clone(), kind), output.clone());
            }
        }
        BuildState { outputs: Mutex::new(outputs) }
    }

    fn insert(&self, id: PackageId, req: Platform,
              output: BuildOutput) {
        let mut outputs = self.outputs.lock().unwrap();
        match req {
            Platform::Target => { outputs.insert((id, Kind::Target), output); }
            Platform::Plugin => { outputs.insert((id, Kind::Host), output); }

            // If this build output was for both the host and target platforms,
            // we need to insert it at both places.
            Platform::PluginAndTarget => {
                outputs.insert((id.clone(), Kind::Host), output.clone());
                outputs.insert((id, Kind::Target), output);
            }
        }
    }
}

impl BuildOutput {
    // Parses the output of a script.
    // The `pkg_name` is used for error messages.
    pub fn parse(input: &str, pkg_name: &str) -> CargoResult<BuildOutput> {
        let mut library_paths = Vec::new();
        let mut library_links = Vec::new();
        let mut cfgs = Vec::new();
        let mut metadata = Vec::new();
        let whence = format!("build script of `{}`", pkg_name);

        for line in input.lines() {
            let mut iter = line.splitn(2, ':');
            if iter.next() != Some("cargo") {
                // skip this line since it doesn't start with "cargo:"
                continue;
            }
            let data = match iter.next() {
                Some(val) => val,
                None => continue
            };

            // getting the `key=value` part of the line
            let mut iter = data.splitn(2, '=');
            let key = iter.next();
            let value = iter.next();
            let (key, value) = match (key, value) {
                (Some(a), Some(b)) => (a, b.trim_right()),
                // line started with `cargo:` but didn't match `key=value`
                _ => return Err(human(format!("Wrong output in {}: `{}`",
                                              whence, line)))
            };

            match key {
                "rustc-flags" => {
                    let (libs, links) = try!(
                        BuildOutput::parse_rustc_flags(value, &whence)
                    );
                    library_links.extend(links.into_iter());
                    library_paths.extend(libs.into_iter());
                }
                "rustc-link-lib" => library_links.push(value.to_string()),
                "rustc-link-search" => library_paths.push(PathBuf::from(value)),
                "rustc-cfg" => cfgs.push(value.to_string()),
                _ => metadata.push((key.to_string(), value.to_string())),
            }
        }

        Ok(BuildOutput {
            library_paths: library_paths,
            library_links: library_links,
            cfgs: cfgs,
            metadata: metadata,
        })
    }

    pub fn parse_rustc_flags(value: &str, whence: &str)
                             -> CargoResult<(Vec<PathBuf>, Vec<String>)> {
        let value = value.trim();
        let mut flags_iter = value.split(|c: char| c.is_whitespace())
                                  .filter(|w| w.chars().any(|c| !c.is_whitespace()));
        let (mut library_links, mut library_paths) = (Vec::new(), Vec::new());
        loop {
            let flag = match flags_iter.next() {
                Some(f) => f,
                None => break
            };
            if flag != "-l" && flag != "-L" {
                return Err(human(format!("Only `-l` and `-L` flags are allowed \
                                         in {}: `{}`",
                                         whence, value)))
            }
            let value = match flags_iter.next() {
                Some(v) => v,
                None => return Err(human(format!("Flag in rustc-flags has no \
                                                  value in {}: `{}`",
                                                  whence, value)))
            };
            match flag {
                "-l" => library_links.push(value.to_string()),
                "-L" => library_paths.push(PathBuf::from(value)),

                // was already checked above
                _ => return Err(human("only -l and -L flags are allowed"))
            };
        }
        Ok((library_paths, library_links))
    }
}

/// Compute the `build_scripts` map in the `Context` which tracks what build
/// scripts each package depends on.
///
/// The global `build_scripts` map lists for all (package, kind) tuples what set
/// of packages' build script outputs must be considered. For example this lists
/// all dependencies' `-L` flags which need to be propagated transitively.
///
/// The given set of targets to this function is the initial set of
/// targets/profiles which are being built.
pub fn build_map<'b, 'cfg>(cx: &mut Context<'b, 'cfg>,
                           pkgs: &'b PackagesToBuild<'b>) {
    let mut ret = HashMap::new();
    for &(pkg, ref targets) in pkgs {
        for &(target, profile) in targets {
            build(&mut ret, Kind::Target, pkg, target, profile, cx);
            build(&mut ret, Kind::Host, pkg, target, profile, cx);
        }
    }

    // Make the output a little more deterministic by sorting all dependencies
    for (&(id, target, _, kind), slot) in ret.iter_mut() {
        slot.sort();
        slot.dedup();
        debug!("script deps: {}/{}/{:?} => {:?}", id, target.name(), kind,
               slot.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    }
    cx.build_scripts = ret;

    // Recursive function to build up the map we're constructing. This function
    // memoizes all of its return values as it goes along.
    fn build<'a, 'b, 'cfg>(out: &'a mut HashMap<(&'b PackageId, &'b Target,
                                                 &'b Profile, Kind),
                                                Vec<&'b PackageId>>,
                           kind: Kind,
                           pkg: &'b Package,
                           target: &'b Target,
                           profile: &'b Profile,
                           cx: &Context<'b, 'cfg>)
                           -> &'a [&'b PackageId] {
        // If this target has crossed into "host-land" we need to change the
        // kind that we're compiling for, and otherwise just do a quick
        // pre-flight check to see if we've already calculated the set of
        // dependencies.
        let kind = kind.for_target(target);
        let id = pkg.package_id();
        if out.contains_key(&(id, target, profile, kind)) {
            return &out[&(id, target, profile, kind)]
        }

        // This loop is both the recursive and additive portion of this
        // function, the key part of the logic being around determining the
        // right `kind` to recurse on. If a dependency fits in the kind that
        // we've got specified, then we just keep plazing a trail, but otherwise
        // we *switch* the kind we're looking at because it must fit into the
        // other category.
        //
        // We always recurse, but only add to our own array if the target is
        // linkable to us (e.g. not a binary) and it's for the same original
        // `kind`.
        let mut ret = Vec::new();
        for (pkg, target, p) in cx.dep_targets(pkg, target, kind, profile) {
            let req = cx.get_requirement(pkg, target);

            let dep_kind = if req.includes(kind) {
                kind
            } else if kind == Kind::Target {
                Kind::Host
            } else {
                Kind::Target
            };
            let dep_scripts = build(out, dep_kind, pkg, target, p, cx);

            if target.linkable() && kind == dep_kind {
                if pkg.has_custom_build() {
                    ret.push(pkg.package_id());
                }
                ret.extend(dep_scripts.iter().cloned());
            }
        }

        let prev = out.entry((id, target, profile, kind)).or_insert(Vec::new());
        prev.extend(ret);
        return prev
    }
}
