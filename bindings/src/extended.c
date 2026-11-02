// Owned binding extensions. Keep the vendored Tree-sitter source unmodified.
#include "../vendor/src/lib.c"

// A match must not depend on its parent or neighboring children when queried
// in a separate subtree. Check both the public flags and the compiled root steps.
// This uses private structs: review it when updating vendor/src/query.c.
bool tree_house_query_can_partition(const TSQuery *query) {
  for (uint32_t p = 0; p < query->patterns.size; p++) {
    if (!ts_query_is_pattern_rooted(query, p) ||
        ts_query_is_pattern_non_local(query, p)) {
      return false;
    }
    Slice steps = query->patterns.contents[p].steps;
    for (uint32_t i = steps.offset; i < steps.offset + steps.length; i++) {
      const QueryStep *step = &query->steps.contents[i];
      if (step->depth == 0 &&
          (step->field || step->supertype_symbol || step->is_immediate ||
           step->is_last_child)) {
        return false;
      }
    }
  }
  return true;
}
