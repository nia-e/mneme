# Synthetic evaluation inputs

These authored scenarios support the retained, provider-free tools tests. Names,
project paths, experiences and labels in the cases are synthetic. Host-only gold
must not be included in actor or selector inputs.

The case bytes are retained unchanged, but their location and harness source have
changed. This source tree is not a historical frozen run, and contains no old run
receipts. Fresh preparation must hash the current inputs and current runtime; do
not reuse a receipt from another source cut. Holdout outputs must not be used to
tune a scored comparison.

Run the offline suite from the repository root with Python 3.11 or newer:

```sh
python3 -B -m unittest discover -s tools -p 'test_*.py'
```

Provider/native probes are separate, opt-in experiments. They may require locally
built binaries, an explicitly prepared model cache, authentication, and new output
paths. Offline tests do not establish installed behavior or task-quality gains.
