use std::process::exit;
use std::path::Path;
use std::env;

use clap;
use cargo_shim::{
    Profile,
    CargoPackage,
    CargoProject,
    CargoTarget,
    BuildType,
    BuildConfig,
    TargetKind,
    CargoResult,
    MessageFormat,
    target_to_build_target
};

use config::Config;
use emscripten::initialize_emscripten;
use error::Error;
use wasm;

pub struct BuildArgsMatcher< 'a > {
    pub matches: &'a clap::ArgMatches< 'a >,
    pub project: &'a CargoProject
}

impl< 'a > BuildArgsMatcher< 'a > {
    fn requested_build_type( &self ) -> BuildType {
        if self.matches.is_present( "release" ) {
            BuildType::Release
        } else {
            BuildType::Debug
        }
    }

    pub fn targeting_emscripten_asmjs( &self ) -> bool {
        !self.targeting_emscripten_wasm() && !self.targeting_native_wasm()
    }

    pub fn targeting_emscripten_wasm( &self ) -> bool {
        self.matches.is_present( "target-webasm-emscripten" )
    }

    pub fn targeting_native_wasm( &self ) -> bool {
        self.matches.is_present( "target-webasm" )
    }

    pub fn targeting_wasm( &self ) -> bool {
        self.targeting_emscripten_wasm() || self.targeting_native_wasm()
    }

    pub fn targeting_emscripten( &self ) -> bool {
        self.targeting_emscripten_wasm() || self.targeting_emscripten_asmjs()
    }

    fn use_system_emscripten( &self ) -> bool {
        self.matches.is_present( "use-system-emscripten" )
    }

    fn message_format( &self ) -> MessageFormat {
        if let Some( name ) = self.matches.value_of( "message-format" ) {
            match name {
                "human" => MessageFormat::Human,
                "json" => MessageFormat::Json,
                _ => unreachable!()
            }
        } else {
            MessageFormat::Human
        }
    }

    fn is_verbose( &self ) -> bool {
        self.matches.is_present( "verbose" )
    }

    fn build_type( &self ) -> BuildType {
        let build_type = self.requested_build_type();
        if self.targeting_native_wasm() && build_type == BuildType::Debug {
            // TODO: Remove this in the future.
            println_err!( "warning: debug builds on the wasm-unknown-unknown are currently totally broken" );
            println_err!( "         forcing a release build" );
            return BuildType::Release;
        }

        build_type
    }

    fn package( &self ) -> Result< Option< &CargoPackage >, Error > {
        if let Some( name ) = self.matches.value_of( "package" ) {
            match self.project.packages.iter().find( |package| package.name == name ) {
                None => Err( Error::ConfigurationError( format!( "package `{}` not found", name ) ) ),
                package => Ok( package )
            }
        } else {
            Ok( None )
        }
    }

    pub fn package_or_default( &self ) -> Result< &CargoPackage, Error > {
        Ok( self.package()?.unwrap_or_else( || self.project.default_package() ) )
    }

    fn target( &'a self, package: &'a CargoPackage ) -> Result< Option< &'a CargoTarget >, Error > {
        let targets = &package.targets;
        if self.matches.is_present( "lib" ) {
            match targets.iter().find( |target| target.kind == TargetKind::Lib ) {
                None => return Err( Error::ConfigurationError( format!( "no library targets found" ) ) ),
                target => Ok( target )
            }
        } else if let Some( name ) = self.matches.value_of( "bin" ) {
            match targets.iter().find( |target| target.kind == TargetKind::Bin && target.name == name ) {
                None => return Err( Error::ConfigurationError( format!( "no bin target named `{}`", name ) ) ),
                target => Ok( target )
            }
        } else if let Some( name ) = self.matches.value_of( "example" ) {
            match targets.iter().find( |target| target.kind == TargetKind::Example && target.name == name ) {
                None => return Err( Error::ConfigurationError( format!( "no example target named `{}`", name ) ) ),
                target => Ok( target )
            }
        } else if let Some( name ) = self.matches.value_of( "bench" ) {
            match targets.iter().find( |target| target.kind == TargetKind::Bench && target.name == name ) {
                None => return Err( Error::ConfigurationError( format!( "no bench target named `{}`", name ) ) ),
                target => Ok( target )
            }
        } else {
            Ok( None )
        }
    }

    pub fn target_or_select< F >( &'a self, package: &'a CargoPackage, filter: F ) -> Result< Vec< &'a CargoTarget >, Error >
        where for< 'r > F: Fn( &'r CargoTarget ) -> bool
    {
        Ok( self.target( package )?.map( |target| vec![ target ] ).unwrap_or_else( || {
            package.targets.iter().filter( |target| filter( target ) ).collect()
        }))
    }

    fn triplet_or_default( &self ) -> &str {
        if self.matches.is_present( "target-webasm") {
            "wasm32-unknown-unknown"
        } else if self.matches.is_present( "target-webasm-emscripten" ) {
            "wasm32-unknown-emscripten"
        } else {
            "asmjs-unknown-emscripten"
        }
    }

    fn features( &self ) -> Vec< &str > {
        if let Some( features ) = self.matches.value_of( "features" ) {
            features.split_whitespace().collect()
        } else {
            Vec::new()
        }
    }

    pub fn prepare_builder( &self, config: &Config, package: &CargoPackage, target: &CargoTarget, profile: Profile ) -> Builder {
        let mut extra_paths = Vec::new();
        let mut extra_rustflags = Vec::new();
        let mut extra_environment = Vec::new();

        if self.targeting_emscripten() {
            if let Some( emscripten ) = initialize_emscripten( self.use_system_emscripten(), self.targeting_wasm() ) {
                extra_paths.push( emscripten.emscripten_path.clone() );

                let emscripten_path = emscripten.emscripten_path.to_string_lossy().into_owned();
                let emscripten_llvm_path = emscripten.emscripten_llvm_path.to_string_lossy().into_owned();

                extra_environment.push( ("EMSCRIPTEN".to_owned(), emscripten_path) );
                extra_environment.push( ("EMSCRIPTEN_FASTCOMP".to_owned(), emscripten_llvm_path.clone()) );
                extra_environment.push( ("LLVM".to_owned(), emscripten_llvm_path) );
                if let Some( binaryen_path ) = emscripten.binaryen_path {
                    let binaryen_path = binaryen_path.to_string_lossy().into_owned();
                    extra_environment.push( ("BINARYEN".to_owned(), binaryen_path) );
                }
            }

            // When compiling tests we want the exit runtime,
            // when compiling for the Web we don't want it
            // since that's more efficient.
            let exit_runtime = profile == Profile::Main;

            extra_rustflags.push( "-C".to_owned() );
            extra_rustflags.push( "link-arg=-s".to_owned() );
            extra_rustflags.push( "-C".to_owned() );
            extra_rustflags.push( format!( "link-arg=NO_EXIT_RUNTIME={}", exit_runtime as u32 ) );
        }

        if let Some( ref link_args ) = config.link_args {
            for arg in link_args {
                if arg.contains( " " ) {
                    // Not sure how to handle spaces, as `-C link-arg="{}"` doesn't work.
                    println_err!( "error: you have a space in one of the entries in `link-args` in your `Web.toml`;" );
                    println_err!( "       this is currently unsupported - aborting!" );
                    exit( 101 );
                }

                extra_rustflags.push( "-C".to_owned() );
                extra_rustflags.push( format!( "link-arg={}", arg ) );
            }
        }

        if self.targeting_native_wasm() && self.requested_build_type() == BuildType::Debug {
            extra_rustflags.push( "-C".to_owned() );
            extra_rustflags.push( "debuginfo=2".to_owned() );
        }

        if self.targeting_native_wasm() {
            // Incremental compilation currently doesn't work very well with
            // this target, so disable it.
            if env::var_os( "CARGO_INCREMENTAL" ).is_some() {
                extra_environment.push( ("CARGO_INCREMENTAL".to_owned(), "0".to_owned()) );
            }
        }

        Builder::new( BuildConfig {
            build_target: target_to_build_target( target, profile ),
            build_type: self.build_type(),
            triplet: Some( self.triplet_or_default().into() ),
            package: Some( package.name.clone() ),
            features: self.features().into_iter().map( |feature| feature.to_owned() ).collect(),
            no_default_features: self.matches.is_present( "no-default-features" ),
            enable_all_features: self.matches.is_present( "all-features" ),
            extra_paths,
            extra_rustflags,
            extra_environment,
            message_format: self.message_format(),
            is_verbose: self.is_verbose()
        })
    }
}

pub struct Builder( BuildConfig );

impl Builder {
    pub fn new( build_config: BuildConfig ) -> Self {
        Builder( build_config )
    }

    pub fn run( &self ) -> Result< CargoResult, Error > {
        let result = self.0.build( Some( |path: &Path| {
            if let Some( artifact ) = wasm::process_wasm_file( &self.0, path ) {
                vec![ artifact ]
            } else {
                Vec::new()
            }
        }));

        if result.is_ok() == false {
            return Err( Error::BuildError );
        }

        Ok( result )
    }
}
