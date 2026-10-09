"""Small shared offline fixtures for the tag classifier and journal tests."""
import json
from tag_context import TagContext

DB="01ARZ3NDEKTSV4RRFFQ69G5FAV"
A="01ARZ3NDEKTSV4RRFFQ69G5FAA"
B="01ARZ3NDEKTSV4RRFFQ69G5FAB"
C="01ARZ3NDEKTSV4RRFFQ69G5FAC"
FP="a"*64
GUIDE="b"*64


def context():
    return TagContext(True,"available",DB,None,json.dumps({"items":[],"partial":False,"coverage":{}}),10)


def target(identifier=A,fingerprint=FP):
    return {"id":identifier,"summary":"A Rust compiler contributor studies type-system invariants.",
            "tags":["rust"],"content_fingerprint":fingerprint}
