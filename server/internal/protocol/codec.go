package protocol

import "encoding/json"

// Encode marshals a typed payload into an envelope frame ready to send.
func Encode(t string, payload any) ([]byte, error) {
	d, err := json.Marshal(payload)
	if err != nil {
		return nil, err
	}
	return json.Marshal(Envelope{T: t, D: d})
}

// DecodeEnvelope parses the outer envelope.
func DecodeEnvelope(b []byte) (Envelope, error) {
	var e Envelope
	err := json.Unmarshal(b, &e)
	return e, err
}

// Decode unmarshals an envelope's payload into v.
func (e Envelope) Decode(v any) error {
	return json.Unmarshal(e.D, v)
}
