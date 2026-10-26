# Blank OOXML fixtures

These blank test fixtures were created for Synck and saved by Jaime Rivas in
Microsoft Word, Excel, and PowerPoint for Mac on 2026-10-26. They are used by the
Office ZIP/chunking tests.

| Fixture | Contents | Application property | AppVersion property |
| --- | --- | --- | --- |
| `empty.docx` | One empty paragraph, Letter page, Word compatibility mode 15 | Microsoft Office Word | `16.0000` |
| `empty.xlsx` | One empty `Sheet1` worksheet | Microsoft Macintosh Excel | `16.0300` |
| `empty.pptx` | One blank 4:3 slide with its layout, master and theme | Microsoft Macintosh PowerPoint | `16.0000` |

These are the files' stored application properties, not full installed Office
release numbers. Their native styles, themes, defaults and font references are
retained. Font names reference installed fonts; no font binaries, media, macros
or external relationships are embedded.

Privacy cleanup removed each document's personal `lastModifiedBy` property and
the workbook's machine-local `x15ac:absPath` element. All other XML part payloads
remain byte-identical to the native saves. The ZIP containers were repacked with
DEFLATE, original part order, fixed timestamps and permissions. Repacking resets
Office's compression-option flags from 6 to 0, so these are sanitized native
packages rather than byte-exact Office archives.

Copyright 2026 Jaime Rivas. These fixtures are distributed under the repository's
[Apache-2.0 license](../../../../LICENSE-APACHE).
