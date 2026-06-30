"""Local transform: extract entities from raw text.

Input  : any entity whose value holds free text (paste a snippet into a Phrase
         entity, or run it on a Notes/description field).
Output : Email / Phone / Domain / IPv4 / Person entities found in that text,
         using the same extractor as the case exporter.

Unlike BadbitchCaseExpand this does NOT touch the case DB — it works purely on
the text you give it, so it's handy for ad-hoc fragments (a WHOIS blob, a page
dump, a paste) before they're ever saved to a dossier.
"""

from maltego_trx.transform import DiscoverableTransform

from .badbitch_common import MALTEGO_TYPE, build_edges, extract_entities


class BadbitchExtractEntities(DiscoverableTransform):
    @classmethod
    def create_entities(cls, request, response):
        text = request.Value or ""
        entities = extract_entities(text)
        if not entities:
            response.addUIMessage("No emails / phones / domains / IPs found in the text.")
            return

        edges = build_edges(entities, text)
        label_by_value = {}
        for edge in edges:
            label_by_value.setdefault(edge.tgt.value, edge.label)

        for ent in entities:
            # Skip the synthetic "Person" guess here — on a raw fragment it's
            # usually noise; BadbitchCaseExpand handles the real subject.
            if ent.ty == "Person":
                continue
            mtype = MALTEGO_TYPE.get(ent.ty, "maltego.Phrase")
            e = response.addEntity(mtype, ent.value)
            e.setWeight(ent.weight)
            link_label = label_by_value.get(ent.value)
            if link_label:
                e.setLinkLabel(link_label)
