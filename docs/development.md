# Development

## Running Tests

If you don't have a Python virtual environment yet, create one.

```bash
# Set up the virtual environment (if not already done)
python -m venv .venv
source .venv/bin/activate  # On Windows: .venv\Scripts\activate
pip install -e .
```

The tests will automatically use the virtual environment.

```bash
cargo test
```
