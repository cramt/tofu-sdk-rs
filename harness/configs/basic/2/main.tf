# Iteration 2: change alpha's content (in-place update) and drop beta (destroy).
# beta.json should disappear; alpha.json should flip to action "updated".
resource "fs_file" "alpha" {
  name = "alpha"
  content = {
    hello = "there"
    extra = "yes"
  }
}
