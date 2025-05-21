import os
from typing import Optional
from .base import BaseCommand


class BuildCommand(BaseCommand):
    """Implementation of the 'build' command."""

    @classmethod
    def register_subparser(cls, subparsers):
        """Register the build command's subparser."""
        build_parser = subparsers.add_parser("build", help="Build using avocado config")
        build_parser.add_argument("config", nargs="?", help="Path to avocado config file (optional)")
        build_parser.add_argument("--wic", "-w", help="Path to WIC (yocto) file (optional)")
        build_parser.add_argument("--out", "-o",
                                help="Directory to write output files (default: current directory)")

    def execute(self, args):
        """Execute the build command."""
        config_file = args.config
        wic_file = args.wic
        out = args.out

        # Set default config file if none provided
        if not config_file:
            config_file = self._find_config_in_cwd()
            if not config_file:
                print("No avocado config file found in current directory.")
                return False

        out = out or os.getcwd()

        print(f"Building with config: {config_file}")
        if wic_file:
            print(f"Using WIC file: {wic_file}")
        print(f"Output directory: {out}")

        # Simulate generating output files
        output_files = ["avocado_output1.bin", "avocado_output2.img"]

        # Check if files exist and ask for overwrite confirmation
        for file in output_files:
            out_path = os.path.join(out, file)
            if os.path.exists(out_path):
                response = input(f"File {file} already exists. Overwrite? (y/n): ")
                if response.lower() != 'y':
                    print(f"Skipping {file}")
                    continue

            # In a real implementation, this is where file creation would happen
            print(f"Generated {out_path}")

        return True

    def _find_config_in_cwd(self) -> Optional[str]:
        """Find an avocado config file in the current working directory."""
        config_names = ["avocado.config", "avocado.cfg", ".avocado"]
        for name in config_names:
            if os.path.exists(name):
                return os.path.abspath(name)
        return None
