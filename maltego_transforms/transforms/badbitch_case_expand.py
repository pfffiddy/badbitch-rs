"""Local transform: expand a badbitch-rs case into a Maltego graph.

Input  : an entity whose value is a saved case's `property_id`
         (use a Phrase or Unknown entity, value = the property_id).
Output : Person / Email / Phone / Domain / IPv4 entities for the case, with the
         relationship labels (email / phone / domain / ip / email-domain /
         co-located) attached as link labels — matching `export_to_maltego`.
"""

from maltego_trx.transform import DiscoverableTransform

from .badbitch_common import MALTEGO_TYPE, graph_for_case


class BadbitchCaseExpand(DiscoverableTransform):
    @classmethod
    def create_entities(cls, request, response):
        property_id = (request.Value or "").strip()
        if not property_id:
            response.addUIMessage("No property_id supplied (set the entity value to a case id).")
            return

        entities, edges = graph_for_case(property_id)
        if not entities:
            response.addUIMessage(
                f"No case '{property_id}' found in the badbitch DB "
                f"(set BADBITCH_DB if the store is elsewhere)."
            )
            return

        # Outgoing-link label for each entity = the label of the first edge that
        # targets it (subject anchor labels dominate; derived edges add detail).
        label_by_value = {}
        for edge in edges:
            label_by_value.setdefault(edge.tgt.value, edge.label)

        for ent in entities:
            mtype = MALTEGO_TYPE.get(ent.ty, "maltego.Phrase")
            e = response.addEntity(mtype, ent.value)
            e.setWeight(ent.weight)
            link_label = label_by_value.get(ent.value)
            if link_label:
                e.setLinkLabel(link_label)

        response.addUIMessage(
            f"badbitch case '{property_id}': {len(entities)} entities, {len(edges)} links."
        )
