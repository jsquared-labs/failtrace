def check_total(value: int) -> None:
    assert value == 3


def test_plain_assert() -> None:
    assert 2 + 2 == 5


def test_assert_in_helper() -> None:
    check_total(4)
