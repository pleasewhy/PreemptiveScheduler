use executor::spawn;

#[test]
pub fn basic_test() {
  spawn(hello());
  executor::run();
}

pub async fn hello() {
  println!("hello waorld!");
}

