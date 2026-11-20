# PDF page count test data

Each file is the same seven-page PDF rewritten by qpdf 12.4.2. The input is a
classic file with one cross-reference table, its catalog at object 1, its page
tree at 2 and its pages at 3 to 9, each `<< /Type /Page /Parent 2 0 R
/MediaBox [0 0 612 792] >>`: the shape `classic_pdf(&refs(&page_tree(7)), 1)`
writes in `../../page_count.rs`'s tests. `pdfinfo` counts seven pages in every
file it can open.

| File | qpdf options | Security handler |
|---|---|---|
| `objstm.pdf` | `--object-streams=generate` | none |
| `rc4-40.pdf` | `--allow-weak-crypto --encrypt "" owner 40 --` | RC4, 40-bit key, revision 2 |
| `rc4-128.pdf` | `--allow-weak-crypto --encrypt "" owner 128 --use-aes=n --` | RC4, 128-bit key, revision 3 |
| `aes-128.pdf` | `--allow-weak-crypto --encrypt "" owner 128 --use-aes=y --` | AES-128 (`AESV2`), revision 4 |
| `aes-256.pdf` | `--allow-weak-crypto --encrypt "" owner 256 --` | AES-256 (`AESV3`), revision 6 |
| `aes-256-r5.pdf` | `--allow-weak-crypto --encrypt "" owner 256 --force-R5 --` | AES-256 (`AESV3`), revision 5 |
| `aes-128-cleartext-metadata-objstm.pdf` | `--allow-weak-crypto --object-streams=generate --encrypt "" owner 128 --use-aes=y --cleartext-metadata --` | AES-128, revision 4, `/EncryptMetadata false` |
| `aes-256-cleartext-metadata-objstm.pdf` | `--allow-weak-crypto --object-streams=generate --encrypt "" owner 256 --cleartext-metadata --` | AES-256, revision 6, `/EncryptMetadata false` |
| `user-password-rc4.pdf` | `--allow-weak-crypto --encrypt user owner 128 --use-aes=n --` | RC4, user password `user` |
| `user-password-objstm.pdf` | `--object-streams=generate --encrypt user owner 256 --` | AES-256, user password `user` |

Each `*-objstm.pdf` beside an encrypted file was written with the same options
and `--object-streams=generate`: its page tree sits in an encrypted object
stream behind a cross-reference stream. `pdfinfo` opens the user-password
files only with their password.
