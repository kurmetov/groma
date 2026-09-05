# rivet-api

A read-only HTTP/JSON API over the models `rivet export-json` produced, so an
agent can be given tools against them and a retrieval index can be fed from
them.

It serves artefacts rather than parsing `.rvt` per request: a full decode costs
seconds and gigabytes, which is not a request handler's business, and serving
the artefact keeps every answer traceable to the export it came from. A decode
improvement reaches the API by re-running the export.

## Running it

```bash
# 1. Produce one artefact per model. Name the file what you want the model
#    called - the file stem is the model id in every route.
mkdir -p data/api
./target/release/rivet export-json path/to/AR_S1.rvt -o data/api/AR_S1.jsonl

# 2. Serve the directory.
./target/release/rivet-api --data data/api
# rivet-api listening on http://127.0.0.1:8787 over data/api (1 model(s))
```

`--addr` changes the bind address (default `127.0.0.1:8787`), `--threads` the
worker count (default 4), `--preload` loads every model at startup instead of
on first use.

A model is loaded on first request and then kept. On AR S1 - 800 133 elements,
a 235 MB artefact - the load takes **0.8 s** and the process settles at about
**740 MB** resident. Afterwards a filtered query answers in single-digit
milliseconds.

## Routes

| Route | What it gives |
|---|---|
| `GET /health` | status, the models on disk, and this route list |
| `GET /models` | model ids |
| `GET /models/{model}/summary` | element counts, and the top classes and categories |
| `GET /models/{model}/elements?…` | filtered, paged elements |
| `GET /models/{model}/elements/{id}` | one element by its Revit element id |
| `GET /models/{model}/rooms` | every room, with its parameters |
| `GET /models/{model}/levels` | the storeys, low to high |
| `GET /models/{model}/documents` | NDJSON, one document per record worth indexing |

Filters on `/elements`, combining with AND:

| Parameter | Meaning |
|---|---|
| `class` | exact Revit class, case-insensitive (`SWall`, `FamilyInstance`, `RoomElem`) |
| `category` | exact category, case-insensitive (`OST_Walls`) |
| `level` | exact storey name (`01 Этаж`) |
| `name` | substring of the element's name |
| `q` | substring of name, class, category, level, type, family and every text parameter |
| `model_elements` | keep only model elements, not type definitions or annotation |
| `with_geometry` | keep only elements carrying recovered geometry |
| `offset`, `limit` | paging; `limit` defaults to 50 and is capped at 1000 |

`total` in the response is the number of matches, not the size of the page.

```bash
curl 'http://127.0.0.1:8787/models/AR_S1/elements?class=SWall&model_elements&limit=5'
curl 'http://127.0.0.1:8787/models/AR_S1/elements?level=01%20%D0%AD%D1%82%D0%B0%D0%B6&with_geometry'
curl 'http://127.0.0.1:8787/models/AR_S1/documents' > index.ndjson
```

## Feeding an index

`/documents` streams NDJSON with no page cap - it is the deliberate bulk route.
Each line is one record worth retrieving, with a prose `text` to embed and the
fields worth filtering on beside it:

```json
{"model":"AR_S1","id":1000001,"kind":"room","text":"Model: AR_S1\nKind: room\nName: 801\nLevel: -01 Подвал\nSP_площадь_приведённая: 19.35 Square meters\nNumber: 801\nName: Помещение\nSP_тип_помещения: Зона\n","class":"RoomElem","level":"-01 Подвал","name":"801","room_number":"801","source":{"partition":"Partitions/749","member":8,"offset":50268}}
```

`kind` is `element`, `room` or `level`. Annotation, styles, view artefacts and
type definitions are left out, so AR S1 yields **17 946** documents from
800 133 records: 17 377 elements, 554 rooms and 15 storeys. `source` locates
the record in the `.rvt` for anything that needs checking against the file.

## Two things to know about the data

**Numbers are in Revit's internal units unless told otherwise.** A parameter's
`double` is raw - feet, square feet, radians. Where the Forge spec was
recovered the parameter also carries `storage_value`, `unit` and `unit_name`,
and the document text uses those. That happens for **project parameters**
(78.3% of them) and never for built-in ones, whose spec is Revit's own
definition and is not in the file. Rooms are mostly project parameters, so
their areas do come out in square metres; a wall's `WALL_USER_HEIGHT_PARAM`
does not. Only converted values reach the indexed prose - a number whose unit
is unknown is left out of it rather than embedded as a bare figure.

**A storey is a level a model element stands on.** The record walk recovers
every `Level` the file mentions, including those a linked model contributes -
721 on AR S1 for 15 real storeys. `/levels` and the indexed documents apply the
exporter's rule, so the API, the IFC and Revit's own export agree on 15.
