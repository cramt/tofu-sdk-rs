# Iteration 2: in-place update — change the meta note, rewrite the first
# section's body, and drop the second section. action becomes "updated".
resource "fs_document" "report" {
  name = "report"

  meta {
    author = "alice"
    note   = "final"
  }

  section {
    heading = "intro"
    body    = "world"
  }
}
