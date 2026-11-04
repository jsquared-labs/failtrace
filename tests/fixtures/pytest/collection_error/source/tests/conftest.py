"""Conftest for failtrace's collection-error fixture.

The import on line 8 below MUST stay at line 8. Moving it changes the
failure_id derivation (primary_location.line) and breaks the v1.3
normative hash assertion (a28a10bf9c212128).
"""

from app.fixtures import sample_db
