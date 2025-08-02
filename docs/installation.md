# Installation

The Avocado CLI is currently a Python package, which must be installed via Git URL. Installing via Git URL requires a virtual environment.

_Note: Commands will use `python` and `pip` executables, but they may be `python3` and `pip3` on your system._

## Install with venv and pip

You can create a virtual environment using the `venv` module, then installing via pip.

```bash
python -m venv path/to/venv
source path/to/venv/bin/activate

pip install git+https://github.com/avocado-linux/avocado-cli.git@0.1.0
```

## Install with pipx

**Alternatively**, installing via [pipx](https://github.com/pypa/pipx) will handle virtual environments for you.

```bash
pipx install git+https://github.com/avocado-linux/avocado-cli.git@0.1.0
```
