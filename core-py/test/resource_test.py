import rust_store_py as native


def test_resource_pure_fns():
    assert native.resource_content_path("ab12cd") == "objects/ab/ab12cd"
    assert native.resource_is_external_url("https://x") is True
    assert native.resource_is_external_url("ab12cd") is False
    assert native.resource_compose_url("ab12cd", {"baseUrl": "https://cdn", "pathTemplate": "/{contentPath}"}) \
        == "https://cdn/objects/ab/ab12cd"
