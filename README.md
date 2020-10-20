# Queue locks

This crate provides the same locks as the module `std::sync` in the Rust standard library, except these ones ensure first-come-first-served fairness. All the type names and method names are the same to make sure switching from standard implementations is easy. Poisoning is fully supported, and the `try_read` and `try_write` methods are lock-free as they are supposed to be.

## Use case example

If a server is expected to execute commands in roughly the same order as they were received, then it makes perfect sense to synchronize the data being accessed in a first-come-first-served way. Imagine a web service for students to hand in homework. HTTP requests are received in this order:

* Student A hands in their solution
* Student B hands in their solution
* A command is automatically sent on the deadline
* Student C hands in their solution

Without a first-come-first-served fairness, the server may grant access to the lock out of order which could lead to this scenario:

* Student C’s solution is accepted
* The deadline is registered
* Student A’s homework is marked overdue
* Student B’s homework is marked overdue

## Features

* [x] `RwLock`
* [ ] `Mutex`

Some uses of `unreachable!` may be replaced with `unreachable_unchecked`.
