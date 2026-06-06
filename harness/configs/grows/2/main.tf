# Iteration 2: add a second key (in-place update -> action "updated").
resource "fs_file" "config" {
  name = "config"
  content = {
    a = "1"
    b = "2"
  }
}
