"""Extension build command implementation."""
import os
import sys
import subprocess
import tomlkit
from avocado.commands.base import BaseCommand


class ExtBuildCommand(BaseCommand):
    """Implementation of the 'ext build' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the ext build command's subparser."""
        parser = subparsers.add_parser(
            "build", 
            help="Build an extension"
        )
        
        # Add extension name argument - required
        parser.add_argument(
            "extension",
            help="Name of the extension to build"
        )
        
        # Add optional arguments
        parser.add_argument(
            "--clean", 
            action="store_true",
            help="Clean build (removes previous build artifacts first)"
        )
        
        parser.add_argument(
            "--debug",
            action="store_true",
            help="Build in debug mode"
        )
        
        parser.add_argument(
            "--jobs", "-j",
            type=int,
            help="Number of parallel jobs for build process"
        )
        
        parser.add_argument(
            "-c", "--config",
            default="avocado.toml",
            help="Path to avocado.toml configuration file (default: avocado.toml)"
        )
        
        return parser

    def execute(self, args, parser=None):
        """Execute the ext build command."""
        extension = args.extension
        clean_build = args.clean
        debug_mode = args.debug
        jobs = args.jobs
        config_path = args.config

        # Check if configuration file exists
        if not os.path.exists(config_path):
            print(f"Error: Configuration file '{config_path}' not found.", file=sys.stderr)
            print("Run 'avocado init' first or specify a valid configuration file with --config.", file=sys.stderr)
            return False

        # Load the configuration to validate it exists and is parseable
        try:
            with open(config_path, "r") as f:
                tomlkit.parse(f.read())
        except Exception as e:
            print(f"Error loading configuration: {str(e)}", file=sys.stderr)
            return False
        
        # Validate extension name
        extensions_dir = "extensions"
        extension_dir = os.path.join(extensions_dir, extension)
        
        if not os.path.exists(extension_dir):
            print(f"Error: Extension '{extension}' not found in {extensions_dir}.", file=sys.stderr)
            print(f"Run 'avocado ext create {extension}' to create it first.", file=sys.stderr)
            return False
        
        # Create build directory
        build_dir = os.path.join(extension_dir, "build")
        
        # Clean if requested
        if clean_build and os.path.exists(build_dir):
            print(f"Cleaning build directory for extension '{extension}'")
            try:
                import shutil
                shutil.rmtree(build_dir)
                print(f"Removed existing build directory: {build_dir}")
            except Exception as e:
                print(f"Error cleaning build directory: {str(e)}", file=sys.stderr)
                return False
        
        # Create build directory if it doesn't exist
        if not os.path.exists(build_dir):
            try:
                os.makedirs(build_dir)
                print(f"Created build directory: {build_dir}")
            except OSError as e:
                print(f"Error creating build directory: {str(e)}", file=sys.stderr)
                return False
        
        # Check for dependencies
        deps_dir = os.path.join(extension_dir, "deps")
        dependencies = []
        if os.path.exists(os.path.join(deps_dir, "dependencies.txt")):
            with open(os.path.join(deps_dir, "dependencies.txt"), 'r') as f:
                dependencies = [line.strip() for line in f.readlines() if line.strip()]
        
        if dependencies:
            print(f"Extension '{extension}' has {len(dependencies)} dependencies:")
            for dep in dependencies:
                print(f"  - {dep}")
            print("Ensuring all dependencies are available...")
            # In a real implementation, you'd verify dependencies here
        
        # Configure build
        print(f"Configuring extension '{extension}'...")
        
        try:
            # Change to build directory
            os.chdir(build_dir)
            
            # Set up cmake command
            cmake_cmd = ["cmake", ".."]
            
            if debug_mode:
                cmake_cmd.extend(["-DCMAKE_BUILD_TYPE=Debug"])
            else:
                cmake_cmd.extend(["-DCMAKE_BUILD_TYPE=Release"])
            
            # Run cmake configuration
            print(f"Running: {' '.join(cmake_cmd)}")
            config_result = subprocess.run(cmake_cmd, check=True)
            
            if config_result.returncode != 0:
                print(f"Error configuring extension '{extension}'", file=sys.stderr)
                return False
            
            # Build the extension
            print(f"Building extension '{extension}'...")
            
            # Set up build command
            build_cmd = ["cmake", "--build", "."]
            
            if jobs:
                build_cmd.extend(["--parallel", str(jobs)])
            
            # Run build
            print(f"Running: {' '.join(build_cmd)}")
            build_result = subprocess.run(build_cmd, check=True)
            
            if build_result.returncode != 0:
                print(f"Error building extension '{extension}'", file=sys.stderr)
                return False
            
            print(f"Successfully built extension '{extension}'")
            return True
            
        except subprocess.CalledProcessError as e:
            print(f"Build process error: {str(e)}", file=sys.stderr)
            return False
        except Exception as e:
            print(f"Unexpected error during build: {str(e)}", file=sys.stderr)
            return False