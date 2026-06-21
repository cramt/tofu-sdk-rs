# In-place update vs. replacement.
#
# Two distinct planning paths exercised against the real server:
#  - Toggling `superuser` is not `force_new`, so it plans as an in-place update
#    and drives `ALTER ROLE` via the provider's `update` handler. If `update`
#    were unsupported or broken the run would error.
#  - Renaming the table is `force_new`, so it plans as a replacement: the old
#    table is dropped and a new one created. A fresh catalog OID would result;
#    we assert the new name took effect and still has a valid OID.

run "create" {
  command = apply
}

run "update_superuser_in_place" {
  command = apply

  variables {
    role_superuser = true
  }

  assert {
    condition     = pg_role.app.superuser == true
    error_message = "toggling superuser should ALTER the role in place"
  }

  assert {
    condition     = pg_role.app.oid > 0
    error_message = "the role should still exist after an in-place update"
  }
}

run "replace_table_via_rename" {
  command = apply

  variables {
    role_superuser = true
    table_name     = "gadgets"
  }

  assert {
    condition     = pg_table.widgets.name == "gadgets"
    error_message = "renaming the table should replace it under the new name"
  }

  assert {
    condition     = pg_table.widgets.oid > 0
    error_message = "the replacement table should have a fresh catalog OID"
  }
}
