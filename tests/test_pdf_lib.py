import pdf_lib
from pdf_lib import Pdf, PdfImage, PageSize


def test_create_empty():
    doc = Pdf.create()
    assert doc.page_count == 0
    assert not doc.is_encrypted
    assert doc.title is None
    assert doc.author is None


def test_add_pages():
    doc = Pdf.create()
    doc.add_page()
    doc.add_page(PageSize.A4)
    assert doc.page_count == 2


def test_insert_page():
    doc = Pdf.create()
    doc.add_page()
    doc.add_page()
    doc.insert_page(1, PageSize.LEGAL)
    assert doc.page_count == 3


def test_remove_page():
    doc = Pdf.create()
    doc.add_page()
    doc.add_page()
    doc.remove_page(0)
    assert doc.page_count == 1


def test_metadata():
    doc = Pdf.create()
    doc.title = "Test Title"
    doc.author = "Test Author"
    assert doc.title == "Test Title"
    assert doc.author == "Test Author"


def test_metadata_methods():
    doc = Pdf.create()
    doc.set_subject("Test Subject")
    doc.set_keywords(["a", "b", "c"])
    doc.set_creator("Test Creator")
    doc.set_producer("Test Producer")


def test_save_and_reload():
    doc = Pdf.create()
    doc.add_page(PageSize.LETTER)
    doc.add_page(PageSize.A4)
    doc.title = "Roundtrip"

    data = doc.save()
    assert isinstance(data, bytes)
    assert len(data) > 0

    doc2 = Pdf.load(data)
    assert doc2.page_count == 2
    assert doc2.title == "Roundtrip"


def test_copy_pages():
    src = Pdf.create()
    src.add_page(PageSize.LETTER)
    src.add_page(PageSize.A4)
    src.add_page(PageSize.LEGAL)

    dst = Pdf.create()
    copied = dst.copy_pages(src, [0, 2])
    assert copied == 2
    assert dst.page_count == 2


def test_object_count():
    doc = Pdf.create()
    assert doc.object_count > 0


def test_repr():
    doc = Pdf.create()
    r = repr(doc)
    assert "Pdf(" in r
    assert "pages=0" in r


def test_extract_images_empty():
    doc = Pdf.create()
    doc.add_page()
    images = doc.extract_images()
    assert images == []


def test_load_invalid_raises():
    import pytest

    with pytest.raises(ValueError):
        Pdf.load(b"not a pdf")


def test_page_sizes_exist():
    assert PageSize.LETTER == (612.0, 792.0)
    assert PageSize.A4 == (595.28, 841.89)
    assert PageSize.LEGAL == (612.0, 1008.0)
    assert PageSize.TABLOID == (792.0, 1224.0)
    assert PageSize.LEDGER == (1224.0, 792.0)
    assert PageSize.A0[0] > PageSize.A1[0] > PageSize.A2[0]
    assert PageSize.EXECUTIVE is not None
    assert PageSize.FOLIO is not None
