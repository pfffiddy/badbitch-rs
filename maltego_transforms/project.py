#!/usr/bin/env python3
"""maltego-trx runner for the badbitch-rs local transforms.

Local-transform usage (what Maltego invokes):
    python3 project.py local BadbitchCaseExpand
    python3 project.py local BadbitchExtractEntities

List discovered transforms:
    python3 project.py list

(Server mode `runserver` is also available via maltego-trx if you ever move to a
TDS, but local transforms are the intended, dependency-light path here.)
"""

import sys

from maltego_trx.registry import register_transform_classes
from maltego_trx.server import application
from maltego_trx.handler import handle_run

import transforms

register_transform_classes(transforms)

handle_run(__name__, sys.argv, application)
