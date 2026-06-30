"""Local transform: pivot from any entity across ALL saved cases.

Input  : an entity whose value is any indicator already in your case store —
         an email, domain, phone, IPv4, or a person's name.
Output : that indicator's 1-hop neighbors (direction-agnostic) drawn from every
         saved dossier, each link labelled `<relationship> (<property_id>)` so
         you can see which case the connection came from.

This is the cross-case graph-traversal transform: drop a single indicator on the
graph and fan out to everything badbitch has ever linked to it.
"""

from maltego_trx.transform import DiscoverableTransform

from .badbitch_common import MALTEGO_TYPE, neighbors_of


class BadbitchExpandEntity(DiscoverableTransform):
    @classmethod
    def create_entities(cls, request, response):
        value = (request.Value or "").strip()
        if not value:
            response.addUIMessage("Set the entity value to expand (email / domain / phone / IP / name).")
            return

        hits = neighbors_of(value)
        if not hits:
            response.addUIMessage(
                f"No saved case connects '{value}' (set BADBITCH_DB if the store is elsewhere)."
            )
            return

        for nb, label, pid in hits:
            mtype = MALTEGO_TYPE.get(nb.ty, "maltego.Phrase")
            ent = response.addEntity(mtype, nb.value)
            ent.setWeight(nb.weight)
            ent.setLinkLabel(f"{label} ({pid})")

        response.addUIMessage(f"'{value}': {len(hits)} connection(s) across saved cases.")
