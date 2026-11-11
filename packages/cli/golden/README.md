# Golden codegen output

`testapp.api.generated.ts` is the real emitter output for
`testapp.manifest.json` (itself `python -m pylair.codegen testapp`
output against `python/pylair/tests/testapp.py`). It is committed so
the generated stub surface can be reviewed in-repo.

Regenerate both after changing testapp, the schema, or the emitter:

```sh
cd python/pylair/tests
PYTHONPATH=../src python3 -m pylair.codegen testapp \
  | python3 -m json.tool > ../../../packages/cli/golden/testapp.manifest.json
PYTHONPATH=../src node ../../../packages/cli/dist/cli.js codegen \
  --module testapp --python python3 \
  --out ../../../packages/cli/golden/testapp.api.generated.ts
```

(Kept outside `test/` so `node --test` doesn't try to execute the
generated `.ts` as a test file.)
