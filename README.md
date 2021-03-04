# Conduit
### A Matrix homeserver written in Rust

#### What is the goal?

A fast Matrix homeserver that's easy to set up and just works. You can install
it on a mini-computer like the Raspberry Pi to host Matrix for your family,
friends or company.


#### Can I try it out?

Yes! Just open a Matrix client (<https://app.element.io> or Element Android for
example) and register on the `https://conduit.koesters.xyz` homeserver.


#### What is it build on?

- [Ruma](https://www.ruma.io): Useful structures for endpoint requests and
  responses that can be (de)serialized
- [Sled](https://github.com/spacejam/sled): A simple (key, value) database with
  good performance
- [Rocket](https://rocket.rs): A flexible web framework


#### What are the biggest things still missing?

- Most federation features (invites, e2ee)
- Push notifications on mobile
- Notification settings
- Lots of testing

Check out the [Conduit 1.0 Release Milestone](https://gitlab.com/famedly/conduit/-/milestones/3).


#### How can I deploy my own?

##### Deploy

Download or compile a Conduit binary, set up the config and call it from somewhere like a systemd script. [Read
more](DEPLOY.md)

##### Deploy using Docker

Pull and run the docker image with

``` bash
docker pull matrixconduit/matrix-conduit:latest
docker run -d -p 8448:8000 -v db:/srv/conduit/.local/share/conduit matrixconduit/matrix-conduit:latest
```

Or build and run it with docker or docker-compose. [Read more](docker/README.md)


#### How can I contribute?

1. Look for an issue you would like to work on and make sure it's not assigned
   to other users
2. Ask someone to assign the issue to you (comment on the issue or chat in
   #conduit:nordgedanken.dev)
3. Fork the repo and work on the issue. #conduit:nordgedanken.dev is happy to help :)
4. Submit a MR

#### Donate

Liberapay: <https://liberapay.com/timokoesters/>\
Bitcoin: `bc1qnnykf986tw49ur7wx9rpw2tevpsztvar5x8w4n`
