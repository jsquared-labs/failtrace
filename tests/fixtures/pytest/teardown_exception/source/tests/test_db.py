import pytest


@pytest.fixture
def db_connection():
    yield "fake_connection"
    raise RuntimeError("connection close failed during teardown")


def test_query(db_connection):
    assert db_connection == "fake_connection"
