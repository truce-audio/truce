# truce Additional Terms

> **TL;DR.** truce is dual-licensed under
> [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT). Build, ship,
> and sell **plug-ins, hosts, and end-user audio software** under
> those licenses — no further permission required. These Additional
> Terms only kick in if you intend to redistribute truce **as a
> commercial audio-plug-in framework to third-party developers**.

The standard dual MIT / Apache-2.0 grant covers the use cases the
truce project is designed for: plug-in authors, host vendors, and
end-user audio software builders. Most users will never need to read
past this paragraph.

These Additional Terms govern one specific use case the standard
grant is not intended to cover: redistributing truce itself as part
of a **commercial framework product** offered to third-party plug-in
developers. For that use, contact the Licensor before shipping; the
boundary, the exemption for free / open-source frameworks, and the
request procedure are below.

---

## 1.  Scope — Framework Products

For the purposes of these Additional Terms, a product is a
**"Framework Product"** if both:

  (i)  it is offered, distributed, sublicensed, or otherwise provided
       to third-party developers as a means of building audio plug-ins
       (CLAP, VST3, VST2, LV2, AU v2, AU v3, AAX, or successor
       specifications) or DAW host integrations; **and**

  (ii) the third-party developer's use of the product results in
       audio software that the third-party developer distributes to
       end users or to further developers.

The following are **not** Framework Products, and their distribution
is fully covered by the standard MIT / Apache-2.0 grant:

  - any audio plug-in, plug-in suite, host, analyzer, validator,
    end-user audio application, or hardware product;
  - any hardware-vendor SDK or runtime that includes truce as an
    internal implementation detail and does not expose truce as
    part of its public developer interface;
  - any personal-use scripting layer, prototyping kit, or research
    library;
  - any wrapper or helper library targeting a specific DSP technique,
    GUI backend, or external dependency, even if useful to other
    plug-in authors as a transitive dependency.

A **Framework License**, granted only by written permission from the
Licensor, is required to redistribute truce as part of a Framework
Product that is also a Commercial Offering (as defined in Section 2
below). Free, OSI-licensed, non-commercial Framework Products are
exempt — see Section 2.

---

## 2.  Exemption — Free, Open-Source Framework Products

The permission requirement in Section 1 does **not** apply to a
Framework Product that simultaneously satisfies all of the following
conditions for the entire period of its distribution:

  (a) The Framework Product's complete source code is published and
      made freely accessible under an OSI-approved open-source
      license. "OSI-approved" means a license appearing on the Open
      Source Initiative's approved-licenses list at
      <https://opensource.org/licenses> at the time of distribution.

  (b) The Framework Product is offered to its third-party developer
      users free of charge. Receiving voluntary donations, patronage
      (e.g. GitHub Sponsors, Open Collective, Patreon), or research
      / hobbyist grants for the Framework Product is permitted under
      this exemption provided no contributor receives those funds as
      consideration for use of the Framework Product.

  (c) The Framework Product is not a Commercial Offering.

A **"Commercial Offering"** means any product, service, or
arrangement that:

  (i)   requires payment, subscription, license fee, royalty, or
        other monetary consideration for access to, use of,
        modification of, or distribution of the Framework Product or
        its functionality;

  (ii)  is dual-licensed by its publisher under commercial terms in
        parallel with the open-source license referenced in (a)
        above;

  (iii) is bundled with, or used to gate access to, a paid product,
        paid support contract, paid hosting service, paid training,
        paid certification, or paid consulting offering where the
        Framework Product itself (rather than the paying customer's
        own audio software produced with the Framework Product) is
        the primary value being sold; or

  (iv)  is offered to the public under any other arrangement whose
        principal purpose is commercial advantage or monetary
        compensation to the Framework Product's publisher.

Framework Products meeting (a)–(c) are covered by the standard MIT /
Apache-2.0 grant — no separate permission required. Authors of such
Framework Products are encouraged (but not required) to notify the
Licensor and may be listed in the project's ecosystem documentation.

If a Framework Product subsequently becomes a Commercial Offering,
the exemption ceases to apply from the date the Commercial Offering
begins. Continued distribution of the Framework Product from that
date requires a written Framework License under Section 3. Audio
software already built with the Framework Product under the prior
exemption is unaffected.

The Licensor reserves the right to maintain a public list of
Commercial Offerings the Licensor has determined to fall outside this
exemption. Inclusion on or omission from such a list does not by
itself extend or revoke the exemption — the terms themselves are the
operative source of truth.

---

## 3.  Framework License Grants (Commercial)

The Licensor is under no obligation to grant a Framework License.
When granted, the Licensor may impose any terms (including but not
limited to attribution, contribution-back, scope limits, financial
terms, revocation conditions, and time limits) as a condition of the
grant. Past grants do not bind future decisions.

Requests for a Framework License should be sent to
`framework-licensing@truce.audio` (or initiated as a private
discussion on the truce-audio GitHub organization, with the
maintainers tagged, if email is inconvenient). A well-formed request
describes:

  (a) the product being built, its developer audience, and what that
      audience receives from it;

  (b) the role truce plays inside the product — core, one backend
      among several, internal implementation detail, or otherwise;

  (c) the distribution and commercialization model under which the
      product reaches its third-party developers; and

  (d) the proposed terms of the requested Framework License,
      including any attribution, time, scope, or financial terms the
      requester is willing to accept.

The Licensor will acknowledge any well-formed request within two to
four weeks of receipt. Acknowledgement is not a grant. The Licensor
reserves the right to deny a request without detailed reasoning.

If you are unsure whether your product is a Framework Product, or
whether the Section 2 exemption applies to you, contact the Licensor
before shipping. Ambiguities in the boundary between the standard
grant and these Additional Terms are to be resolved in favor of the
standard MIT / Apache-2.0 grant unless the use is plainly within the
Framework Product definition without satisfying the Section 2
exemption.

---

## 4.  Trademarks

The names "truce", "Truce", "truce-audio", any associated logos, and
any successor identifiers are unregistered trademarks of the
Licensor. These Additional Terms grant no license to these
trademarks; the Apache-2.0 trademark non-grant clause (Section 6 of
Apache-2.0) likewise applies and is preserved.

You may factually refer to the software by its name (e.g. "built with
truce") and you may keep the project's name in vendored or forked
source code as required by the Apache-2.0 notice-preservation clause.
You may not use the name to identify a forked, modified, or competing
product, nor to suggest endorsement or sponsorship by the Licensor.

---

## 5.  Definitions

**"Licensor"** means the truce project maintainers, acting through
the contact channels listed in the project README. Where a Framework
License grant is required under Section 1, the grant must be in
writing from a maintainer with authority to bind the project.

**"Framework Product"** is defined in Section 1.

**"Commercial Offering"** is defined in Section 2.

**"You"** and **"Your"** have the meanings assigned in the
Apache-2.0 license.

Apache-2.0 terms otherwise govern interpretation where these
Additional Terms reference Apache-2.0 mechanics.
