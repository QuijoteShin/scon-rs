# CHANGELOG.md

## 1.1.0 (2026-10-26)

- Tablas con referencia a schema: `key[N]@s:Nombre:` usa las claves de primer nivel del schema, en orden de declaración, como columnas. Cambios en el encoder, el decodificador (owned y borrowed) y el registro de schemas.
- El detector de minificado no trata un `;` dentro de comillas como separador (1.0.2).
- Pruebas nuevas: `tabular_schema_ref` y ampliaciones de `auto_extract` y `roundtrip`.

## 1.0.1 (2026-03-16)

- Primera versión publicada en crates.io junto a 1.0.0.
