# Modeling JSON-LD and tree-shaped data

mnestic stores relations, not nested documents. The most useful migration pattern is
to keep the source document intact while projecting the parts you query or join into
ordinary relations:

- one relation per object type;
- one child row per nested object, linked by its parent's ID;
- one row per array element, with an explicit zero-based position; and
- a node relation plus an adjacency relation when the tree is heterogeneous.

The examples below are complete CozoScript statements. Run each block in order in a
fresh database.

## Keep the source document

Start with a raw JSON relation. `parse_json()` turns a string into mnestic's `Json`
value, so the original payload remains available while the relational projection is
introduced incrementally.

```cozoscript
:create raw_document {doc_id: String => body: Json}
```

```cozoscript
?[doc_id, body] <- [[
  'person:ada',
  parse_json('{"@id":"person:ada","@type":"Person","profile":{"name":"Ada Lovelace","address":{"street":"12 St James Square","city":"London"}},"interests":["mathematics","poetry"]}')
]]
:put raw_document {doc_id => body}
```

`get()` accepts either one object key or a list representing a path. It returns a
normal scalar for JSON strings, numbers, booleans, and nulls, so these values can be
filtered and joined directly:

```cozoscript
?[doc_id, name, city] :=
  *raw_document{doc_id, body},
  name = get(body, ['profile', 'name']),
  city = get(body, ['profile', 'address', 'city'])
```

Use `maybe_get()` instead when a missing key should produce `null` rather than fail
the query. `json(value)` converts a CozoScript scalar or list back to a `Json` value;
for example, `json(['Person', 'Researcher'])` produces a JSON array suitable for a
`Json` column.

## Nested objects: parent and child relations

Give every object a stable ID. Store the parent ID on the child row as the relational
equivalent of a foreign key. mnestic does not enforce foreign-key constraints, so the
application should create and delete the related rows in the same transaction.

```cozoscript
:create person {person_id: String => name: String}
```

```cozoscript
:create address {address_id: String => person_id: String, street: String, city: String}
```

```cozoscript
?[person_id, name] :=
  *raw_document{doc_id: person_id, body},
  name = get(body, ['profile', 'name'])
:put person {person_id => name}
```

```cozoscript
?[address_id, person_id, street, city] :=
  *raw_document{doc_id: person_id, body},
  address_id = concat(person_id, ':address'),
  street = get(body, ['profile', 'address', 'street']),
  city = get(body, ['profile', 'address', 'city'])
:put address {address_id => person_id, street, city}
```

Joining the child back to its parent is then ordinary Datalog:

```cozoscript
?[name, city] :=
  *person{person_id, name},
  *address{person_id, city}
```

## Arrays: one row per element

Do not store a queryable array as one opaque value. Emit one row per element and make
the zero-based position part of the key. This preserves duplicates and source order.

```cozoscript
:create person_interest {person_id: String, position: Int => interest: String}
```

```cozoscript
?[person_id, position, interest] <- [
  ['person:ada', 0, 'mathematics'],
  ['person:ada', 1, 'poetry']
]
:put person_interest {person_id, position => interest}
```

```cozoscript
?[position, interest] := *person_interest{person_id: 'person:ada', position, interest}
:order position
```

`get()` can inspect a known JSON-array position during migration—for example,
`get(get(body, 'interests'), 0)`—but CozoScript does not currently enumerate the
members of an arbitrary `Json` array. Expand dynamic arrays in the ingesting binding
and write the rows in one transaction. A future flattening helper, if added, should
produce the same `(owner_id, position, value)` shape rather than introduce a second
storage model.

## Heterogeneous trees: nodes and adjacency

When different child positions can contain different kinds of value, use one node
relation and one adjacency relation. The essential edge is
`(parent_id, child_id, key)`; the optional `position` distinguishes ordered array
children. Use `key` for object properties and `position` for array elements.

```cozoscript
:create tree_node {node_id: String => kind: String, value: Any?}
```

```cozoscript
:create tree_edge {parent_id: String, child_id: String => key: String?, position: Int?}
```

```cozoscript
?[node_id, kind, value] <- [
  ['person:ada', 'object', null],
  ['person:ada:name', 'string', 'Ada Lovelace'],
  ['person:ada:interests', 'array', null],
  ['person:ada:interest:0', 'string', 'mathematics'],
  ['person:ada:interest:1', 'string', 'poetry']
]
:put tree_node {node_id => kind, value}
```

```cozoscript
?[parent_id, child_id, key, position] <- [
  ['person:ada', 'person:ada:name', 'name', null],
  ['person:ada', 'person:ada:interests', 'interests', null],
  ['person:ada:interests', 'person:ada:interest:0', null, 0],
  ['person:ada:interests', 'person:ada:interest:1', null, 1]
]
:put tree_edge {parent_id, child_id => key, position}
```

Recursive traversal now works across every node kind without changing the schema:

```cozoscript
descendant[root, child] := *tree_edge{parent_id: root, child_id: child}
descendant[root, child] := descendant[root, middle], *tree_edge{parent_id: middle, child_id: child}

?[child_id, kind, value] :=
  descendant['person:ada', child_id],
  *tree_node{node_id: child_id, kind, value}
:order child_id
```

## Choosing a shape

Prefer typed parent/child relations when the object types are stable: their columns
are self-documenting and easy to index. Prefer node-plus-adjacency when the shape is
open-ended or recursive. In both cases, retaining `raw_document` gives you a lossless
fallback and lets you migrate field by field instead of requiring an all-at-once
conversion.
