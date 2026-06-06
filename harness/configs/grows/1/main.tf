# Iteration 1: create a single config file with one key.
resource "fs_file" "config" {
  name = "config"
  content = {
    a = "1"
  }
}
