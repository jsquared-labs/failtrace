"""Failing cases match v1.3 schema example canonical messages:
empty-string=58af4b4b1fcb1036, trailing-newline=b0782aebe12ab236,
unicode-bom=1bbb49abdd90e17e, shared test_group_id=49507ef118ce763f.
"""

import pytest


def parse(value):
    return value  # identity — the deliberate bug


CASES = [
    ('', None, 'empty-string'),
    ('foo\n', 'foo', 'trailing-newline'),
    ('﻿foo', 'foo', 'unicode-bom'),
    ('a', 'a', 'passing-01'),
    ('b', 'b', 'passing-02'),
    ('c', 'c', 'passing-03'),
    ('d', 'd', 'passing-04'),
    ('e', 'e', 'passing-05'),
    ('f', 'f', 'passing-06'),
    ('g', 'g', 'passing-07'),
    ('h', 'h', 'passing-08'),
    ('i', 'i', 'passing-09'),
    ('j', 'j', 'passing-10'),
    ('k', 'k', 'passing-11'),
    ('l', 'l', 'passing-12'),
    ('m', 'm', 'passing-13'),
    ('n', 'n', 'passing-14'),
]


@pytest.mark.parametrize(
    "value,expected",
    [(value, expected) for value, expected, _ in CASES],
    ids=[param_id for _, _, param_id in CASES],
)
def test_parse(value, expected):
    result = parse(value)
    assert result == expected, f"expected {result!a} to equal {expected!a}"
