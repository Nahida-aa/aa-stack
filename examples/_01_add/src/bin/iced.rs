use iced::{Element, widget};

fn main() {
    iced::application(boot, update, view).run().unwrap();
}

struct Counter {
    value: i32,
}

fn boot() -> Counter {
    Counter { value: 42 }
}

#[derive(Clone)]
enum Message {
    Add,
    Sub,
}

fn update(counter: &mut Counter, message: Message) {
    match message {
        Message::Add => counter.value += 1,
        Message::Sub => counter.value -= 1,
    }
}
fn view(counter: &Counter) -> Element<'_, Message> {
    let add_button = widget::button(widget::text("+").size(100)).on_press(Message::Add);
    let sub_button = widget::button(widget::text("-").size(100)).on_press(Message::Sub);
    let text = widget::text(counter.value).size(50);
    widget::column![text, add_button, sub_button].into()
}
